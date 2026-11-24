# Write view filters and ignore files

Lore has two filters with one syntax. A [view filter](../glossary.md#view-filter) is the inbound filter: it decides which paths Lore writes into your [working tree](../glossary.md#working-tree). An [ignore file](../glossary.md#ignore-file) is the outbound filter: it decides which untracked paths you can stage and commit. This guide assumes you know `.gitignore`.

## One syntax, two files

| | View filter | Ignore file |
| --- | --- | --- |
| **Lives in** | `.lore/view` in the [instance](../glossary.md#repository-instance) | `.loreignore` at the repository root, and `.lore/ignore` in the instance |
| **Set with** | `lore clone --view <file>`, then only `lore sync --view <file>`. Never edit `.lore/view`. | Editing the files. Commit `.loreignore` to share it. |
| **Applies to** | Cloning, sync, branch switch, and reset. Stage, and unstage outside a layer mount, see only what it keeps unless forced. Status lists what it keeps and what is staged past it. | Status, stage, dirty marking, locks, and the sizes `lore file info` reports, for paths neither committed nor staged (with `--revision`, paths that revision doesn't hold), and the destination of `lore stage move`. Not cloning, sync, branch switch, or reset. |

- `.lore/ignore` holds exclusions for your instance alone, and is never committed. Its rules win over re-inclusions in `.loreignore`. Lore skips a `!` re-inclusion in it with a warning.
- As in Git, ignore rules apply only to untracked paths: status still reports a committed or staged file's edits and deletion, stage takes them, and lock commands and the sizes `lore file info` reports include the file. The exception is `lore stage move`, which refuses a destination a rule matches, even for a tracked file, unless you give `--force`. Add an ignored file with `lore --force stage <path>`. On a directory, `--force` stages everything the directory holds but Lore's own entries, unchanged files included. Remove a tracked one by deleting it and running `lore stage <path>`.
- `lore --force stage` reaches past the view as well, and the commit takes such a file from your working tree. A merge stages its changes at paths the view excludes too, and the commit takes those as the merge staged them, unless you force-stage the file: that resolves the merge with your file, as editing a merged file in view does. Status lists both as staged. `lore unstage` follows the view and leaves them staged; `lore --force unstage` takes back what you force-staged outside a merge. Below a layer mount, `lore unstage` doesn't follow the view yet.
- Lore also ignores, as built-in rules, anything named `.lore` or `.urc` at any depth, and names ending in `~mine`, `~theirs`, or `~base` (merge copies), or `.~loretemp`. A forced stage takes only the `~mine` ones.

## Rule syntax

One rule per line, matched against paths from the repository root, as in [gitignore](https://git-scm.com/docs/gitignore) in the Git documentation. The examples use this repository:

```text
Docs/guide.md
Engine/Build/build.cs
Engine/Config/base.ini
Engine/Content/a.uasset
Engine/Content/Deep/b.uasset
Engine/Docs/readme.txt
Engine/Source/Editor/editor.cpp
Engine/Source/Runtime/core.cpp
Engine/Source/Runtime/Private/impl.cpp
Engine/Source/ThirdParty/zlib/zlib.c
Makefile
README.md
Samples/Demo/demo.txt
Samples/Demo/docs
```

| Rule | Matches | Example |
| --- | --- | --- |
| `# text` | Nothing. Blank lines are skipped too. | `# Build output` |
| `\#name`, `\!name` | A name starting with `#` or `!` | `\#notes.txt` |
| `!rule` | Re-includes what an earlier rule excluded. The last matching rule wins. | `*.cpp` then `!core.cpp` keeps `core.cpp` |
| `/path` | That path from the root | `/Docs/` matches `Docs/`, not `Engine/Docs/` |
| `name` | That name at any depth, when the rule has no `/` except a trailing one | `core.cpp` matches `Engine/Source/Runtime/core.cpp` |
| `a/b` | That path from the root: a `/` in the middle anchors too | `Engine/Docs/` |
| `rule/` | Directories only | `Docs/` matches the directories `Docs/` and `Engine/Docs/`, not the file `Samples/Demo/docs` |
| `*` | Any characters within one path component | `/Engine/Source/Runtime/*.cpp` matches `core.cpp`, not `Private/impl.cpp` |
| `?` | One character within a component | `/Engine/Source/*/core.?pp` matches `Runtime/core.cpp` |
| `**` | Any number of components | `/Engine/**/Build/` matches `Engine/Build/` |

A path no rule matches keeps its directory's verdict.

## Name a directory or what it holds

A rule ending in a name, or in a name and `/`, matches that directory itself. A rule ending in `/*` or `/**` matches what the directory holds, and not the directory.

| Rule | As an exclusion | As a re-inclusion |
| --- | --- | --- |
| `/a` | Drops `a` and everything in it | Brings back `a` and everything in it |
| `/a/` | The same, if `a` is a directory | The same, if `a` is a directory |
| `/a/*` | Drops everything in `a`, and keeps `a` | Brings back the entries of `a`, and leaves `a` excluded. After `**`, what lies below the entries stays excluded. |
| `/a/**` | Drops everything in `a`, and keeps `a` | Brings back everything in `a`, and leaves `a` excluded |

Write `/a/` and `!/a/` for a directory, and `/a/*` to drop what a directory holds and keep the directory. Under an ignore file, `/a/*` and `/a/**` leave `a` itself unignored, so staging takes it empty.

As exclusions, `/a/*` and `/a/**` differ only below a later re-inclusion that brings back the entries of a directory in `a` without what lies below them: `/a/**` matches at every depth, so after `/a/**` and `!/a/b/*` the entries of `a/b` are kept and what lies below them is not, where after `/a/*` it is.

Under a view, Lore creates every directory the view keeps, whatever it holds. A directory the view excludes is on disk only as the parent of what Lore creates below it: a file or directory the view keeps, or a link or layer mount. Lore walks such a directory only where a later re-inclusion could match below it, and creates it only where it finds something kept, so after `/*` and `!/a/**` the directory `a` stays off the disk while it is empty. `lore clone`, `lore sync`, `lore branch switch`, `lore branch merge`, `lore revision cherry-pick`, `lore revision revert`, `lore sync --view`, `lore reset`, `lore layer add`, and `lore link add` follow this, and `lore status` lists a directory as deleted only where they create it. A sync, branch switch, merge, cherry-pick, or revert removes an excluded directory once nothing Lore creates is left in it, and keeps one holding an untracked or ignored file. Inside a link's mount, a sync that doesn't reset the working tree and a branch switch keep every directory. A virtual instance lists every directory the view keeps, and every excluded directory Lore walks.

A directory the view keeps is one `lore stage` sees, so deleting it is a deletion like any other: `lore status` lists the directory as deleted, `lore stage` stages it as deleted, and everything below it goes with it, the files the view leaves out included, as part of the directory's deletion rather than for being absent. Committing removes them all from the revision. In a repository that holds a link, staging deletes only what the view keeps below such a directory, and the directory stays while it holds a link or anything the view leaves out, so `lore status` goes on listing its deletion. `lore unstage` takes the deletion back, and `lore reset` creates the directory again.

## Differences from `.gitignore`

- **Only the root `.loreignore` counts.** One in a subdirectory has no effect.
- **Matching ignores case**, and Lore trims spaces at both ends of a line. Git matches case on a case-sensitive file system, and keeps leading spaces.
- **Re-inclusion works below an excluded directory**, as [Re-include one file deep inside an excluded tree](#re-include-one-file-deep-inside-an-excluded-tree) shows, unless a later rule excludes a directory on the way: adding `/Engine/*/` to the end of that example leaves `impl.cpp` out, so put the re-inclusion after `/Engine/*/` to keep it.
- **A path re-inclusion covers its subtree.** After `**`, `!/Engine/Config/` brings back `base.ini` too. One ending in `*`, such as `!/*`, re-includes only what it matches, so after `**` what lies below it stays excluded.
- **A path re-inclusion that starts with `**`, such as `!**/Saved/Config/`, is refused.** `lore clone --view` and `lore sync --view` refuse a view holding one. Under a `.loreignore` holding one, every command in the instance fails until you remove the line:

  ```text
  [Error] Failed to load repository filter: filter inclusions cannot start with a ** component as that will force traversal of the entire revision tree
  ```

## Best practices

### Exclude everything, then re-include

```text
/*
!/Engine/Config/
```

The instance holds `Engine/Config/base.ini` and nothing else. Re-inclusions alone change nothing, and Lore warns `Filter only has inclusions but no exclusions`.

### Anchor rules with a leading `/`

Start a rule with `/` unless you mean every file or directory of that name.

### Start every re-inclusion below an excluded directory with a literal path

A re-inclusion that starts with a wildcard, such as `!/*/Config/`, makes Lore look inside every directory the wildcard matches, here each top-level directory. A name re-inclusion, such as `!Config/` or `!**/Config/`, is accepted but doesn't reach into an excluded directory.

### Ignore build outputs by name

With this `.loreignore`:

```text
*.tmp
Intermediate/
Binaries/
```

Name rules apply at any depth. In this tree:

```text
Game/Binaries/game.exe
Game/Intermediate/obj.o
Game/Source/main.cpp
Game/cache.tmp
Plugins/Audio/Binaries/audio.dll
Plugins/Audio/Intermediate/audio.o
Plugins/Audio/Source/audio.cpp
```

`lore status --scan` reports, and `lore stage --scan .` stages, only the sources and `.loreignore`:

```text
Untracked files:
A .loreignore
A Game/
A Game/Source/
A Game/Source/main.cpp
A Plugins/
A Plugins/Audio/
A Plugins/Audio/Source/
A Plugins/Audio/Source/audio.cpp
```

## Change a view with `lore sync --view`

```bash
lore sync --view config.view
```

`lore sync --view` is a full sync: it also moves the instance to the branch's latest revision. It refuses while anything is staged, and when the sync would merge a diverged branch. Lore writes the files entering the view, deletes those leaving it, and keeps untracked files. If that would delete a file you've modified, it refuses and changes nothing. From an instance with no view filter, with `Engine/Source/Runtime/core.cpp` modified, and `config.view` holding the view from [Exclude everything, then re-include](#exclude-everything-then-re-include):

```text
[Error] Deleted directory has modified files in file system: Engine/Source
[Error] Deleted file is currently modified in file system: Engine/Source/Runtime/core.cpp, size 45 mtime <mtime>, revision mode 0 size 39 hash <hash>
[Error] Local modifications prevent synchronization
```

## Advanced examples

Each example shows what an instance holds or status reports.

### Keep root files without the root directories

```text
/*/
```

```text
Makefile
README.md
```

### Keep a directory's own files without its subdirectories

```text
/*
!/Engine/Source/Runtime/
/Engine/Source/Runtime/*/
```

```text
Engine/
Engine/Source/
Engine/Source/Runtime/
Engine/Source/Runtime/core.cpp
```

`!/Engine/Source/Runtime/*` alone brings `Private/impl.cpp` too: below `/*`, which excludes only the top level, what lies below a re-included entry follows the entry.

### Keep an otherwise empty directory

```text
/*
!/Engine/Content/
/Engine/Content/*
```

```text
Engine/
Engine/Content/
```

`!/Engine/Content/` keeps the directory, so the instance holds it though `/Engine/Content/*` excludes everything in it.

### Exclude a directory without leaving it behind

```text
/*
!/Engine/Content/
/Engine/Content/Deep/
```

```text
Engine/
Engine/Content/
Engine/Content/a.uasset
```

`/Engine/Content/Deep/*` excludes what `Deep/` holds and keeps `Deep/`, so the instance holds it empty, and an ignore file leaves it to stage empty.

### Re-include one file deep inside an excluded tree

```text
/*
!/Engine/Source/Runtime/Private/impl.cpp
```

```text
Engine/
Engine/Source/
Engine/Source/Runtime/
Engine/Source/Runtime/Private/
Engine/Source/Runtime/Private/impl.cpp
```

### Keep one subdirectory of an ignored directory

In this tree:

```text
Game/Saved/Config/game.ini
Game/Saved/Logs/game.log
Game/Source/main.cpp
```

With this `.loreignore`:

```text
Saved/
!Saved/Config/
```

The middle `/` in `!Saved/Config/` anchors it to the root, so `Game/Saved/Config/game.ini` stays ignored. Write the path from the root:

```text
Saved/
!/Game/Saved/Config/
```

```text
Untracked files:
A .loreignore
A Game/
A Game/Saved/
A Game/Saved/Config/
A Game/Saved/Config/game.ini
A Game/Source/
A Game/Source/main.cpp
```

### Write a complete engine-source view

```text
# Root files only: exclude every root directory
/*/

# The engine source and what it builds with
!/Engine/Build/
!/Engine/Config/
!/Engine/Source/
/Engine/Source/ThirdParty/

# One sample
!/Samples/Demo/
```

```text
Engine/
Engine/Build/
Engine/Build/build.cs
Engine/Config/
Engine/Config/base.ini
Engine/Source/
Engine/Source/Editor/
Engine/Source/Editor/editor.cpp
Engine/Source/Runtime/
Engine/Source/Runtime/Private/
Engine/Source/Runtime/Private/impl.cpp
Engine/Source/Runtime/core.cpp
Makefile
README.md
Samples/
Samples/Demo/
Samples/Demo/demo.txt
Samples/Demo/docs
```

## See also

- [`lore clone`](../reference/lore-cli-commands.md#lore-clone) and [`lore sync`](../reference/lore-cli-commands.md#lore-sync) — the `--view` option.
- [View files](../explanation/system-design.md#122-view-files-inbound-filtering), [ignore files](../explanation/system-design.md#123-ignore-files-outbound-filtering), and [FilterMode](../explanation/system-design.md#124-filtermode-which-filter-applies-when) — why Lore filters in two directions.
