# Parallel path staging

`file::stage::stage` takes a set of user paths and walks them against the file system concurrently. This page describes the pipeline those paths run through — layer routing, expansion, normalization, shared-ancestor pre-creation, and the target fan-out — and the invariant each stage establishes for the next. The pipeline lives in `lore-revision/src/file/stage.rs`. The walk each target runs is `stage::stage_filesystem_path` in `lore-revision/src/stage.rs`.

The staging concurrency rests on one property of the node tree: `State::node_add` is always-create, not get-or-add. Two tasks adding the same `(parent, name)` produce duplicate siblings. Every stage below exists to reach a target set where that can't happen.

## Pipeline

```text
user paths
    │
    ├── classify_stage_path ─────────► layer jobs      (own repository and state)
    │
    ├── expand_stage_target ─────────► dirty paths under a directory target
    │
    ├── dedup_to_supersets ──────────► antichain       one case variation per entry,
    │                                                  no path covered by another
    │
    ├── shared ancestors ────────────► directories two or more targets share,
    │                                                  grouped by depth
    │
    ├── resolve_prefixes ────────────► on-disk case per shared ancestor
    │
    ├── pre-create ──────────────────► one depth at a time,
    │                                                  concurrent within a depth
    │
    └── target fan-out ──────────────► every target walked from its deepest
                                                       pre-created ancestor, concurrent
```

## Layer routing and expansion

Each user path is made repository-relative, then classified against the layer mounts.

| Route                    | Meaning                                     | Destination                                   |
| ------------------------ | ------------------------------------------- | --------------------------------------------- |
| `LayerRoute::Inside`     | The path is inside a layer mount.           | A layer job, with the suffix below the mount. |
| `LayerRoute::AncestorOf` | The path is above one or more layer mounts. | The main target set, plus one job per layer.  |
| `LayerRoute::Disjoint`   | The path touches no layer.                  | The main target set.                          |

Main-repository paths pass through `expand_stage_target`. Without `--scan`, a path that resolves to an existing directory node expands to the dirty paths beneath it. Every other path expands to itself.

Layer jobs run against their own repository and state, and are spawned after the main targets. Every layer mount is masked out of every main-repository walk, so a mount is staged by its own job and never by a parent walk.

## Target normalization

`RelativePath::dedup_to_supersets` reduces the main target set to an antichain and settles the case of the components its paths share.

The ordering is subtree order over the lowercase form of each path, tie-broken by subtree order over the original. That order ranks `/` below every other byte, so a directory is immediately followed by its own subtree and never by an unrelated sibling. Taking the lowercase form first puts the case variations of one entry next to each other.

Two reductions run over that order, against the last path kept:

| Reduction             | Rule                                                                                         |
| --------------------- | -------------------------------------------------------------------------------------------- |
| **Case unification**  | A path takes the kept path's case across the leading components whose lowercase forms match. |
| **Covering collapse** | A path equal to, or beneath, the kept path is dropped.                                       |

A repository-root path among the expanded targets short-circuits both: the antichain becomes the root alone, which covers the whole tree.

The result holds one case variation of each entry and no path covered by another, and is returned in lexicographic order. Unification is what the concurrency below depends on — a parent node holds a single entry of a name, so two case variations of one directory name one node.

## Shared ancestors

The shared ancestors are read off the ordered antichain rather than counted. Targets under a directory are contiguous in lexicographic order, so a directory is shared exactly when two neighbours agree that far, and emitting it at the first target of its run yields the set once over. The result is sorted by depth and then lexicographically.

| Property                    | Established by                                                              | Consequence                                         |
| --------------------------- | --------------------------------------------------------------------------- | --------------------------------------------------- |
| **Prefix-closed**           | An ancestor two targets share has its own ancestors shared by the same two. | Every parent of a depth sits in the depth above it. |
| **Emitted once**            | Each is emitted at the first target of its run.                             | The set needs no deduplicating.                     |
| **One variation per entry** | Inherited from the antichain.                                               | No two entries of a depth name one node.            |
| **Sorted by depth**         | The sort key.                                                               | One depth is a contiguous range of the vector.      |

Together these make a depth a set of distinct nodes whose parents already exist, which is what allows a depth to be created at once.

An ancestor beneath a single target is left out. One walk reaches it, so nothing races to create it.

## Prefix resolution

`util::fs::resolve_prefixes` maps each shared ancestor to the case the file system holds it in, so no target beneath it resolves that directory again. Resolution runs against the repository root and the keys are repository-relative, so the map answers for that base and no other.

Resolution also runs a depth at a time. A run of paths at one depth resolves against parents at shallower ones, so nothing in a run waits on anything else in it. Within a run, consecutive paths sharing a parent reuse one resolved case and one directory path.

A prefix resolves to one case variation or to none. Three cases produce none, and all three leave it out of the map:

| Case                  | Probe result                                                               |
| --------------------- | -------------------------------------------------------------------------- |
| **Absent**            | No child of the parent matches the name, in any case.                      |
| **Ambiguous**         | Two or more children match, which only a case-sensitive file system holds. |
| **Unreadable parent** | The parent couldn't be read, because it's missing or the read failed.      |

A path beneath a missing prefix resolves as it would with no map at all: `filesystem_path` walks the components itself, and where a component is ambiguous it forks over the variations and picks the one that carries the rest of the path. A walk starting below the root reads the map differently — see Where a walk starts.

No map is built under `StageCaseChange::Keep`. That mode stages by renaming the file system to match the tree, and the first such rename would leave the map naming a directory that's no longer there.

## Where a walk starts

Pre-creation and the target fan-out both spawn their walks through `walk_base`, which starts each one at the deepest ancestor that already has a node:

| Case                             | Base absolute path   | Base relative path | Base node   | Walked                 |
| -------------------------------- | -------------------- | ------------------ | ----------- | ---------------------- |
| An ancestor above it was created | `<repo>/<variation>` | `<variation>`      | that node   | the remainder below it |
| None was                         | `<repo>`             | empty              | `ROOT_NODE` | the whole path         |

`variation` is the resolved case when the prefix map covers the whole prefix, and the requested one otherwise. A map entry for a shorter prefix answers for a shorter path, so taking it would drop the components between.

The requested case is what a walk starting below the root gets whenever the map has no entry for its prefix — under `StageCaseChange::Keep`, which builds no map, and for the three cases above that leave a prefix out of one. The base is where a walk starts rather than a component it resolves, so a walk given a base the file system doesn't hold can't resolve its target at all. It takes the path-not-found branch, which stages a delete when the tree holds the path and reports an invalid path when it doesn't.

The prefix map is passed to a walk starting at the root and withheld from one starting lower down, because its keys are repository-relative and a path relative to a prefix isn't.

## Depth-level pre-creation

The shared ancestors are created one depth at a time. A depth is spawned into one task set, and the next depth starts only once that set has drained.

Each walk runs with `no_children`, so it creates its own final component and nothing beneath it.

### Error and cancellation handling

Dropping a `JoinSet` aborts its tasks, and a pre-create in flight is allocating nodes. Every depth therefore drains to completion whether or not a walk failed. The first error is kept and returned once the depth is drained. No further depth starts.

Only a walk that returns a valid node link is recorded. A filtered-out or deleted ancestor is left out of the map, and its descendants start from the nearest recorded ancestor above them, or from the root.

## Target fan-out

Every antichain entry is then spawned as a walk, from the same base its shared ancestors left it. That chain already exists and is already resolved, so the walk creates only what's unique to it and skips a metadata syscall and a node lookup per component it would otherwise re-resolve. Every remaining creation is either single-writer or a distinct sibling, both of which `node_add` supports concurrently.

Layer jobs are appended to the same task set once the main targets are spawned.

## Case along the pipeline

A component carries four case variations along the pipeline, and only the last reaches the tree.

| Stage        | Case variation                              | Set by                                                           |
| ------------ | ------------------------------------------- | ---------------------------------------------------------------- |
| **Given**    | Whatever the caller typed.                  | The caller.                                                      |
| **Unified**  | The first of the given variations in order. | `dedup_to_supersets`.                                            |
| **Resolved** | What the file system holds.                 | `filesystem_path`, through the prefix map where it has an entry. |
| **Staged**   | What the node carries.                      | The case mode.                                                   |

Unification decides which entry a target names. The case mode decides how that entry is cased:

| Mode         | Outcome                                                         |
| ------------ | --------------------------------------------------------------- |
| **`Error`**  | A disagreement between the file system and the tree is refused. |
| **`Keep`**   | The file system is renamed to match the tree.                   |
| **`Rename`** | The tree takes the name the file system holds.                  |

A caller having typed a third case variation troubles none of them. Resolving the given path against the file system settles that before the node is consulted.

## Concurrency limits

Three bounds cap in-flight work. Each exists because the set being iterated can hold hundreds of thousands of entries, and spawning one task per entry costs task state before any work happens.

| Bound                 | Site                     | Value  | Bounds                                      |
| --------------------- | ------------------------ | ------ | ------------------------------------------- |
| `MAX_TASKS`           | `resolve_prefixes`       | `1000` | Prefixes resolved at once.                  |
| `MAX_PRECREATE_TASKS` | Depth-level pre-creation | `1000` | Ancestors created at once within one depth. |
| `MAX_TASKS`           | Target fan-out           | `1000` | Targets walked at once.                     |

The pre-creation and prefix-resolution loops reap finished tasks with `try_join_next` before blocking on `join_next` at the cap. The target loop uses `lore_limit_drain_tasks!`, which does the same.

## Worked example

Ten targets, inconsistently cased, against a file system holding `Assets/Meshes/{rock,tree}.mesh`, `Assets/Textures/{bark,leaf}.png`, `Config/Maps/{a,b}.umap`, and `Docs/readme.md`.

Normalization, in the order the paths sort:

| Sorted target              | Unified                    | Kept        |
| -------------------------- | -------------------------- | ----------- |
| `ASSETS/Meshes/rock.mesh`  | first, unchanged           | yes         |
| `Assets/Meshes/rock.mesh`  | `ASSETS/Meshes/rock.mesh`  | no, equal   |
| `assets/meshes/tree.mesh`  | `ASSETS/Meshes/tree.mesh`  | yes         |
| `Assets/Textures/bark.png` | `ASSETS/Textures/bark.png` | yes         |
| `Assets/Textures/bark.png` | `ASSETS/Textures/bark.png` | no, equal   |
| `assets/TEXTURES/leaf.png` | `ASSETS/Textures/leaf.png` | yes         |
| `Config/Maps`              | unchanged                  | yes         |
| `Config/Maps/a.umap`       | unchanged                  | no, covered |
| `config/maps/b.umap`       | `Config/Maps/b.umap`       | no, covered |
| `Docs/readme.md`           | unchanged                  | yes         |

`ASSETS` wins the unification because `S` sorts before `s` on the tie-break. The winner is the first variation in the order, not the one on disk.

Ancestor counts over the six survivors, and the depths they group into:

```text
ASSETS            4  ✓          depth 1   ASSETS
ASSETS/Meshes     2  ✓          depth 2   ASSETS/Meshes, ASSETS/Textures
ASSETS/Textures   2  ✓
Config            1  ✗
Docs              1  ✗
```

Prefix resolution, then the two depths:

```text
map     ASSETS → Assets   ASSETS/Meshes → Assets/Meshes   ASSETS/Textures → Assets/Textures

depth 1   ASSETS            base <repo> / "" / ROOT_NODE          creates Assets
          ── drain ──

depth 2   ASSETS/Meshes     base <repo>/Assets / "Assets" / node  creates Meshes   ┐ together
          ASSETS/Textures   base <repo>/Assets / "Assets" / node  creates Textures ┘
          ── drain ──
```

The six targets then walk together, each from its own base:

```text
ASSETS/Meshes/rock.mesh    base <repo>/Assets/Meshes   / "Assets/Meshes"   / node       adds rock.mesh
ASSETS/Meshes/tree.mesh    base <repo>/Assets/Meshes   / "Assets/Meshes"   / node       adds tree.mesh
ASSETS/Textures/bark.png   base <repo>/Assets/Textures / "Assets/Textures" / node       adds bark.png
ASSETS/Textures/leaf.png   base <repo>/Assets/Textures / "Assets/Textures" / node       adds leaf.png
Config/Maps                base <repo>                 / ""                / ROOT_NODE  creates both
Docs/readme.md             base <repo>                 / ""                / ROOT_NODE  creates both
```

The four under `ASSETS` add a leaf to a node the pre-creation handed them. `Config/Maps` and `Docs/readme.md` are under no shared ancestor, so they walk from the root and create their own parents, which no other walk reaches.

## Source pointers

- `lore-revision/src/file/stage.rs::stage` — the pipeline.
- `lore-revision/src/file/stage.rs::walk_base` — where a walk starts.
- `lore-revision/src/file/stage.rs::collect_precreate` — folds a finished pre-creation into the ancestor node map.
- `lore-revision/src/util/path.rs::RelativePath::dedup_to_supersets` — normalization and case unification.
- `lore-revision/src/util/fs.rs::resolve_prefixes` — the shared-ancestor case map.
- `lore-revision/src/stage.rs::stage_filesystem_path` — the walk one target runs.
- `lore-revision/src/state.rs::State::node_add` — the concurrency contract every stage above serves.

## See also

- [File I/O engine](file-io-engine.md) — the driver and syscall pool the walks wait on.
