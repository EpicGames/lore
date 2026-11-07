# State node architecture

A `State` is one revision tree held in memory: a committed revision a command reads, the staged tree that staging or a C API handle edits, or the working tree a virtual file system (VFS) provider serves. This page describes, at the level of its architecture, how that tree is stored, how it changes while many tasks read and write it at once, and how it stays both correct and fast while they do. The storage is the node block in `lore-revision/src/state/block.rs` (re-exported as `lore_revision::node`), and the code that loads, registers and saves blocks is in `lore-revision/src/state.rs`. The node operations in `lore-revision/src/state/node/` are the code that changes nodes, their chains and their flags; outside them, only saving and loading a State (with committing, where it resets the root record before saving), and clearing the flags left above a removed node, write node blocks. The VFS provider for Linux, the C API's revision-tree verbs, staging, committing and the other commands all change trees through them.

## Overview

Three demands shape the tree: a command touches only part of a large tree, many tasks change it at once, and any of them may be dropped at any await. So the tree is a set of fixed-size blocks of fixed-size node records, and a block loads from the store on demand and stays in memory while something holds it. A directory's children form a chain threaded through their records, so no directory owns an array or an index that a change must rewrite.

Concurrency rests on five ideas:

- **Small, ordered critical sections:** Each block has its own reader/writer lock, and no lock covers the whole tree. An operation write-locks only the few blocks it changes, in ascending block order, and never holds a lock across an await.
- **Searches without locks, decisions under them:** Whatever an operation has to find, it finds with no write lock held, then checks it again under its locks. Per-directory change counts and change logs show whether a chain moved meanwhile. When a check fails, the operation tries again, a bounded number of times.
- **Tiers by caller:** Callers fall into tiers with different guarantees, chiefly VFS requests on a served State and everything else. The served tier keeps a stronger invariant at every moment, because its callers run beside the provider's own background work on the tree.
- **Ordered steps:** Any operation may be dropped at any await. Each orders its steps so that dropping it leaves the tree consistent, at worst with a slot left for sweeping or a directory flagged dirty without need, but for the cases under Left to callers.
- **Freeing only where nothing runs:** Another task may still hold the ID of a node taken out of its chain, so its record stays in place, and sweeping frees it at a point where nothing else runs on that State.

Performance comes from the same structure: readers copy out under short read guards, writers change one or a few blocks, hot paths allocate nothing, and the differences between tiers are resolved at compile time rather than tested at runtime.

## The tree in memory

### Records, blocks and node IDs

Finding a node from its ID should cost no lookup, so a node ID is the node's place: the index of its block and its slot within the block.

A node is a fixed-size record (`Node`), laid out the same in memory and in the store. It holds the node's kind and flags (dirty and staged flags with their action bits, and `Discarded`), its extension flags, its chain fields (`parent`, `sibling` and `child`, a directory's first child), the location and hash of its name, and its content: mode, size and content address.

A block (`NodeBlock`) holds a fixed number of records (`BLOCK_NODE_COUNT`) behind a small header and is stored as one fragment. The first slot of the first block is the root (`ROOT_NODE`).

A record flagged `Discarded` isn't live. It has been taken out of its chain, is in flight between allocation and publication, waits to be freed after the C API's delete verb detached it, or sits on its block's free list. A record detached or left in flight holds its slot until sweeping frees it.

### Chains

A change to a directory shouldn't rewrite an array of its children, so the children form a chain threaded through their own records. A directory's `child` names its first child, each child's `sibling` names the next, and each child's `parent` names the directory. There's no child count and no index; finding a name walks the chain comparing name hashes. A chain crosses blocks:

```text
node id: (block index, slot)

block 0                                          block 1
  (0,0)  /        child → (0,3)                    (1,0)  b.txt     sibling → (0,4)
  (0,3)  src      child → (1,1), sibling → (1,0)   (1,1)  main.rs   sibling → (1,2)
  (0,4)  a.txt    sibling → none                   (1,2)  lib.rs    sibling → none

chain of /      src → b.txt → a.txt
chain of src    main.rs → lib.rs
```

New children always go to the front of a chain, and so does a node moved into the directory. Callers rely on this: a caller holding a chain it walked earlier knows that every child added since lies ahead of the first child it saw.

### Names and file metadata

A walk compares name hashes and never needs the names themselves, so each block keeps its nodes' names in a name table of its own, stored as a separate fragment that a walk never loads. A record taken out of its chain, or still in flight, keeps its name until sweeping frees its slot, and a save rewrites a table that names were appended to without the names of free slots. File metadata, the custom metadata and the origin of a copy, move or merge, lives in parallel file metadata blocks, indexed like the node blocks, with locks of their own and no chains.

### Block data held in memory only

Some data matters only while a block is in memory: what writers check each other against, what saving and sweeping need to know. A block carries it beside its records, the store never sees it, and it's rebuilt from nothing when the block loads again:

| Data | Purpose |
| --- | --- |
| **Dirty flag and edit sequence** | Whether the block holds edits the store lacks, and whether an edit landed after saving took the block's image. |
| **Chain change counts** | A weighted count of the changes to each directory's chain, which lookups are checked against. |
| **Chain logs** | For a directory a VFS listing watches, a ring of its recent chain changes, which the listing replays instead of rebuilding itself. |
| **Pending clears** | One bit per directory that settling is about to clean. |
| **Registration** | Whether the State holds the block for the next time it saves. |
| **Sweep flag** | Whether the block may hold a detached or leaked slot, so sweeping reads only the blocks that may hold one. |

Because counts and logs start over when a block is loaded again, every count or log position a caller keeps (`ChainMark`, `Absent`) carries the block it was read from, and a check compares the block's identity as well as the number.

### Residency, loading and saving

A command touches only part of a tree, so blocks load on demand and stay in memory only while something holds them. The State holds a `Weak` reference to each loaded block, and a block stays while a bounded block cache (`MAX_BLOCK_CACHE`), the registration of a dirty block until the State saves it, or a caller such as an iterator or a VFS listing holds it. Loading a block takes one of a fixed set of loading permits (`BLOCK_LOADING_PERMITS`), the block's index modulo their count, so two tasks never read the same block twice, and installs the block unless another task installed it first. A block that loads flags itself for sweeping, as an earlier session may have left a detached slot in it.

A writer flags its block dirty under the block's lock, and registers it with the State only when the block turns from clean to dirty, and only after releasing every block lock it holds. Writing to a block that's dirty already touches no State lock at all.

Saving writes every registered block, each under a hold that shuts out writers but not readers and turns into the write lock only for the moment it records the block's image, then the block lists, the tree header and the State's own data. Each block records the edit sequence its image captured, so releasing the blocks afterward turns a block clean only where no edit landed after its image. No writer runs while the State saves. Saving stores a detached or leaked slot as it stands, so staging and committing sweep before they save.

### The write boundary

A writer that skips one of an operation's steps corrupts a tree without any error, so the boundary is enforced by the compiler rather than by review. A node block's write methods are visible only inside the State's own module, where the block type is defined, so a writer outside the State doesn't compile. Tests write through `write_for_tests` and the write-path items a `test-util` build makes public, such as the locks a section takes; a production build has neither. An operation that changes a node in place sees it through `NodeContent`, which can change the node's content and some of its flags, but not its place, name, kind or chain, nor flag it dirty or `Discarded`.

## Tiers

### Why the tiers exist

Callers differ in what runs beside them. A VFS provider serves a working tree to many processes at once: each file system request is an operation on the State, and requests from different processes run concurrently. The provider also works on the tree in the background. After the working tree is committed, it cleans the directories whose contents now match the new revision, and when the working tree switches to another revision, it rewrites the tree in place, removing subtrees that hold no local changes. Both kinds of background work trust the dirty flags: a directory that reads as clean is taken to hold no local change below it. So on a served State, a directory above a dirty node must read as dirty at every moment, even while a request is half done or has just been dropped.

Other callers don't need that guarantee at every moment. A command, staging or a C API handle runs without the provider's background work beside it, and keeps its flags in order by the time its work ends. Holding every caller to the served guarantee would cost them extra locking and extra marking for nothing. Holding no caller to it would let the provider clean a directory with local changes, or remove a subtree with local changes inside it. Tiers give each caller the guarantees it needs and no more.

### The tiers

| Tier | Entry points | Callers | Guarantee |
| --- | --- | --- | --- |
| **Served** | `State::served()` | VFS requests on a State a provider serves | Every operation keeps the served chain rule, so dirty flags are consistent at every moment and after any cancellation. |
| **Swap** | `State::swap()` | The provider switching a served State to another revision in place | Its caller publishes only clean copies from the other revision and detaches only clean nodes, empty directories and local files at their recorded place, none of which can break the served chain rule, so in place of the rule the tier refuses only a copy that isn't clean and a parent flagged `Discarded`. |
| **Unserved** | Methods on `State` | The C API's verbs, staging, committing, the filesystem scan and the other commands | Flags the parent and its ancestors dirty before detaching or reattaching a node. Moving a node leaves flagging both parents to its caller, which flags them after moving it, or before for the C API's move verb; adding a node leaves flagging to its caller, which also keeps each added name distinct, but for the C API's add verb, which flags them itself before publishing. |
| **Discard** | `State::node_discard_pending` | The C API's delete verb, detaching a subtree the same handle added | Flags nothing, since adding the subtree's root flagged its parent already. |

Saving a State is no tier: it runs no node operation, and no writer runs beside it.

The VFS provider for Linux serves a State through the served and swap tiers, and `state_bench` and the stress suite drive them as it does. It doesn't sweep that State yet, so a slot a request left detached stays until the next mount rebuilds the State.

Freeing a slot that another task still holds would hand that task a different node, so the caller's claim on the State decides when slots may be freed. Under a shared claim, many calls run at once and no published slot is ever freed, so every node ID in flight stays valid. Only under an exclusive claim, as when committing or once a command's parallel walks have finished, are published slots freed. A served State holds no exclusive claim, so nothing frees its published slots but sweeping where the provider has paused its requests.

Bodies shared between tiers are generic over a `Tier` trait. The trait's constants and associated flagging steps select each tier's behavior at compile time, so each tier gets its own copy of a shared body with no tier test at runtime, and each copy's future holds only its own tier's state.

## The operations

Each kind of change needs its own order of steps to stay correct beside readers, other writers and cancellation, so the operations fall into a few families, all built from the same pieces: lookups without locks, short sections over a few blocks, and dirty flags set on directories.

- **Attaching a node:** A new node goes in three steps. A slot is taken and flagged `Discarded`. The record is filled in under its block's lock, where everything that can fail happens. Then the node is published: under the locks of the parent's block and the node's block, the operation checks what its caller asked for, clears `Discarded` and prepends the node to the chain. Nothing can see the node before it's published, and an operation dropped while attaching leaves its slot for sweeping rather than a live node outside every chain. Attaching with a check refuses a name a live child holds, and checks its lookup against the parent's change count, or against its chain log where a VFS listing has shown the name absent.
- **Building a copy:** Switching revisions copies whole subtrees. The copy is built out of every chain, with no checks or counting since nothing can reach it, and its root is published once at the end, checked as attaching with a check is.
- **Detaching a node:** The operation reads the node, tests its condition (always, at a given place, only if clean, only if empty, or only if the directory is unchanged since it was read) on that copy, and walks to the node's predecessor. It flags the parent as its tier requires: before the walk on the unserved tier, after it on a served State, where the parent doesn't admit the change already. Then it decides under the locks of the parent's, the predecessor's and the node's blocks. The detached node keeps its record and its `sibling` until sweeping frees it, so a reader that copied its predecessor walks on through it.
- **Moving and renaming:** Moving a node is one critical section over both parents, the node and its predecessor, and a node it replaces and that node's predecessor where there is one: a bounded set of blocks (`MAX_LOCKED_BLOCKS`). In that one section the node leaves its chain, takes its new parent and name, and goes to the front of the new chain, so it's never in no chain and never in two. A directory reattached to another parent, or moved by the C API's move verb, takes the State's moves permit and is checked to stay outside its own subtree; the commands' moves (`node_move`) leave that to their caller. Renaming in place keeps the node's position.
- **Rewriting content:** Changing a file's content takes only the node's block lock and touches no chain. On a served State it also read-locks the directory's block, to check the served chain rule.
- **Flagging directories dirty:** On the unserved tier the flags go up bottom-up, from a node to the root, setting the full flags on the node and only the base flag above, and stopping at the first ancestor already flagged. On a served State they go down top-down instead: the operation climbs on copies to the first ancestor that already admits a served change, then descends one level at a time under a pair of locks, so no step leaves a clean directory above a dirty one.
- **Clearing flags and settling:** Clearing drops a node's flags and then each ancestor's that nothing below still needs, checking the children again after each directory so a concurrent flagging is never lost. Settling, which cleans a served directory after committing or switching revisions, runs in three phases: flag the directory as about to be cleaned, under its lock; compare its children with the committed directory's, with no write lock held; then clean it under the lock, only where that flag survived and the chain is unchanged. A served change below the directory meanwhile drops the pending flag, so settling may be overtaken but is never wrong.
- **Changing a node in place:** Small field changes run a closure on one node under its block's lock, only while the node still stands where the caller found it. Owned variants serve a caller beside which nothing moves or frees the node.
- **Discarding nodes:** Published slots are freed only under an exclusive claim: the subtree's root is detached first, then the subtree is freed, directory by directory or, once a walk has collected it, a block at a time. The C API's delete verb runs under a shared claim and can't free anything, so it detaches the subtree, flags every node in it `Discarded` and queues the root on a lock-free queue, and committing through the C API frees the queue when it sweeps, under its exclusive claim.
- **Allocating slots:** Inside a block, free slots form a list through their `sibling` fields. Blocks with free slots form the tree's unused-block chain. Taking a slot needs only the lock of the first block on that chain, which every grab takes in turn, and flags the block for sweeping, as the slot stays in flight until its node is published; the rarer transitions, retiring a full block, allocating a fresh one, returning a slot that was never published and threading blocks while saving, take the State's unused-slot permit.
- **Sweeping detached slots:** Nothing else frees a slot detached, or left in flight by a call dropped or failed, so sweeping frees such slots at points where nothing else runs on the State. It scans each flagged block in memory once, taking its flag, for slots flagged `Discarded` that hold a node, walks each such root's subtree through the chains its nodes kept, never the root's own `sibling`, which is the chain it left, and frees every subtree it read whole in one pass; a root it can't read whole flags its block again. The unserved form (`State::node_sweep_detached`) first frees the subtrees the C API's delete verb queued; staging runs it at its end, committing through the CLI after discarding nodes, and committing through the C API after its last refusal, each before saving. The served form (`ServedNodes::node_sweep_detached`) runs where a VFS provider has paused its requests and its background work and holds no copy it hasn't published, and keeps whole every detached subtree holding a node that the provider's `retained` test says the kernel still references.

## Concurrency

### Per-block locks in ascending order

Operations on different blocks shouldn't wait for each other at all, so each block has a lock of its own, and every operation takes block locks in one order, so that no two operations ever wait on each other in a cycle. Most operations write-lock one block. An operation that changes a chain takes several blocks as one section (`BlockLocks`), sorted by index and each taken once. A served operation that reads a parent while writing a child takes a `PairLock`: the child's block write-locked and the parent's read-locked, in ascending order. The global order is the State's runtime lock, then block locks in ascending index. A section flags its blocks dirty under its locks and registers them with the State only once every lock is dropped, which keeps the runtime lock out of every section.

### Nothing held across an await

A future may be dropped at any await, and a task parked at one mustn't stall the tasks waiting for its locks, so only saving, with no writer beside it, holds a block lock across an await. Block guards aren't `Send`, so a `Send` future holding one across an await doesn't compile. The owned guards are `Send`: the hold and the read guard that saving keeps on a block across its store writes, with no writer beside it, and a name read in place, which is dropped before any await. Neither is held across a call that may take another block lock. Every other operation releases every lock it takes before its next await, so a dropped future never strands a lock and a waiting task never holds one.

### Searches without locks, decisions under them

A walk meets blocks in chain order, not in index order, and may load them from the store, so it can't hold write locks without breaking the lock order or holding them across an await. So an operation does its searching with no write lock held: walking a chain to find a name or a predecessor, reading a parent, checking a subtree. It then takes its locks and checks again what it relied on.

- **Change counts:** Each directory has a count of the changes to its chain, read together with the directory's record, so a walk's starting point and its count belong together. Adding or detaching a child adds one to the count. Moving a node out of the directory, or renaming it to a different name hash, adds a far larger weight (`REATTACH_CHANGES`). A lookup that reaches the end of a chain without its name reads the count again. If the count grew by less than that weight, nodes were only added or detached since the walk, and the miss stands, as if the lookup had run before them. If it grew by that weight or more, a node may have moved to the front of the chain or taken a new name behind the walk, holding the name the lookup missed, so the lookup walks again. An operation acting on a miss then checks under its locks that the count hasn't moved since the lookup read it, and tries again where it has.
- **Chain logs:** A directory a VFS listing watches keeps a ring of its recent changes. The listing replays the changes since it was built instead of rebuilding itself, and can show a name absent at a log position (`Absent`). Attaching with that proof skips the walk and checks the log under the locks. A log too short to cover the proof sends the operation back to walking.
- **Reclaim count:** Once sweeping frees a slot, taking a slot may hand it out again, so a node ID that a chain mark (`ChainMark`) or a proof of absence (`Absent`) recorded may name another node afterward. Each records the State's reclaim count, which sweeping moves where it frees a slot, and answers stale once the count moved, so a listing rebuilds itself rather than trust a reused slot.
- **Bounded retries:** When a check fails, the operation starts its attempt over, a bounded number of times (`CHAIN_CHANGE_ATTEMPTS`, and `CHAIN_WALK_RESTARTS` for walks), after which it answers `SlowDown`. No command retries it, so the command fails with it. The C API reports it as `LORE_ERROR_CODE_SLOW_DOWN` on the entry and the call, for a C API host to retry. Repeated misses with an unchanged count are told apart from churn and reported as a broken hierarchy.

### Walks beside writers

Readers never wait for writers of other blocks, so a reader walks a chain one node at a time, copying each under its block's read lock while writers change the chain (`ChainWalk`). A detached node keeps its `sibling` until sweeping frees it, so the walk passes through it. A node moved elsewhere, or a cycle guard tripping while the count moves, sends the walk back to the first child. The walk reports when it may have visited a node twice, so a consumer that needs each child once drops the repeats. A walk may miss a node moved to the front behind it, as reading a directory may miss an entry renamed meanwhile.

### The served chain rule

Settling and switching revisions trust a served State's dirty flags at every moment, so on a served State every operation of the served tier that changes a chain or flags a node dirty requires, under the parent's block lock, a parent not flagged `Discarded`, dirty and not about to be cleaned; the root always qualifies. Switching revisions is exempt, as its copies and the nodes it detaches are clean. Operations test the rule first on the copy of the parent they already hold and flag the parent dirty where it fails; the test under the locks then decides.

The rule gives a served State dirty closure at every moment and after any cancellation: every live ancestor of a live dirty node, but the root, is dirty. Two things rest on it. Detaching a clean directory while switching revisions never takes a dirty node with it, and settling can't clean a directory while a change lands below it. Listings stay consistent by a separate mechanism: every chain change a listing can see goes through the node operations, which count and log it, so a listing either replays every change or finds itself stale and rebuilds, as it also does once sweeping freed a slot.

### Ordered steps

Any operation may be dropped at any await, so each orders its steps to leave the tree consistent wherever it stops:

- **Unpublished until complete:** A new node stays `Discarded` until it's published, so dropping the operation leaves a slot for sweeping instead of exposing a partial node.
- **Condition before flags:** Detaching under a condition tests the condition on its copy of the node before flagging anything, so a node that fails it changes nothing. A node that passes there and fails the test under the locks, having changed meanwhile, leaves its parent flagged dirty without need.
- **Flags before changes:** The C API's add, modify, move and delete verbs flag the directories above their node before changing it; restoring a node flags them right after, as deleting it flagged them already. A call dropped between the two leaves directories flagged with no change below them, which a guard notes on the State so that committing through the C API counts only nodes carrying an action.
- **Write before read:** Flagging writes a node before reading its parent, and clearing writes a directory before reading its children again, so concurrent flagging and clearing never lose a dirty flag between them.
- **Register after release:** A block registers with the State only after its locks are dropped, so no registration happens under a block lock and none lands under a clean State.

### Permits for the rare orders

Semaphores order the few changes that a block lock can't:

- **Moves permit:** One per State. Two directories moving into each other's subtrees could each pass the subtree check and together form a cycle cut off from the root, so a directory reattached to another parent, or moved by the C API's move verb, holds the permit from its check until it has moved. The C API's move and delete verbs hold it too, so they never interleave, and so does restoring a node below an added directory, so that deleting the directory can't run beside it.
- **Unused-slot permit:** One per State, over the unused-block chain's transitions.
- **Loading permits:** A fixed set per State (`BLOCK_LOADING_PERMITS`), so tasks loading the same block at once read it from the store only once.

### Left to callers

Some guarantees would cost every caller for the sake of a few, so the contract leaves them to the callers that need them, or accepts the gap. The module documentation of `lore-revision/src/state/node/mod.rs` states each one precisely; in short:

- **Distinct names:** Outside the served tier, adding a node and moving one don't refuse a name already held, so their callers keep each parent and name distinct; attaching with a check refuses one.
- **Flagging dropped or failed midway:** An `Unserved` operation dropped midway can leave a dirty node below a clean directory, and so can flagging that fails midway because it can't load a block: the levels below stay flagged and those above don't, so a later change stops at the flagged levels and committing misses it. The C API's verbs flag the directories above a node before they change it, but for restoring a node, which flags them right after it changes the node, so a call dropped once its flagging is done leaves none of this, though one dropped while its flagging loads a block, or failing on a block it can't load, still can. A C API deletion dropped part way is exempt, as its batch runs in a task a dropped call doesn't stop; one failing on a block it can't load isn't.
- **Clearing above a removed node:** Clearing the flags left above a removed node reads an ancestor's children and then clears the ancestor with nothing held between, so it runs only where nothing flags beside it: once staging's walks have finished, and in `lore dirty`'s loop over its paths, one path at a time.
- **Staging's own listing:** Staging relies on no other task writing a directory while it holds a listing of it. Staging works on a State of its own, and each of its tasks lists and writes only its own directory, so nothing breaks this today but a repository linked at two mount paths, whose two staging tasks share one linked State and may both add one name.
- **Count wraparound:** A count is 32 bits and moving a node out adds 2^16, so 65,536 moves out of one directory bring its count back where it started. A lookup whose walk spans that many sees no change in the count, and could answer that a name is absent while a node moved behind its walk holds it.
- **Linked and layer States:** Staging and committing sweep the State they work on, not the linked and layer States they save beside it, so a slot a call left detached or in flight there stays until that State is swept.
- **Sweeping a served State:** The provider sweeps only where it has paused its requests, is neither settling directories nor switching revisions and holds no copy it hasn't published, and its `retained` test keeps every node the kernel still references. The State checks neither. Until the provider sweeps, a served State's name tables keep the name of every node it detached.
- **Committing through a VFS provider:** If committing through a provider stops once the new revision may be current but before its directories are settled, removing or renaming an added file or directory records a deletion for the rest of the mount.
- **Saving:** A writer running while the State is saved isn't lost, though the saved revision is then no snapshot of one moment. Nothing saves a State beside writers today; if anything did, taking a slot while the State saves could leave a block threaded onto the unused-block chain twice, closing a cycle, and closing that gap would cost every slot allocation a check.
- **The C API:** The C API's move verb flags its destination before moving, so no clearing may run beside it, and none does. A call of the move verb dropped after moving its node leaves the moved subtree without its staged-move flags, so its files commit with no record of moving in their history. Closing a handle doesn't wait for a dropped call's aborted tasks, which only read the store and change the closed tree's in-memory State.

## Performance

### Principles

The node operations are on every command's path and every request of a VFS provider, so small costs add up across all of them:

- **Short read guards that copy out:** A reader copies the record it needs under a read guard and releases it at once. A reader that needs a name in place gets it in a synchronous closure, which can't hold the guard across an await, or as an owned guard (`NodeNameLock`) it drops before its next await, and a walk reads a run of siblings in one block under one guard.
- **Few blocks per operation:** Most operations lock one block, chain changes a bounded few (`MAX_LOCKED_BLOCKS`), and nothing takes a tree-wide lock. Operations on different blocks proceed in parallel.
- **No lock where an atomic suffices:** Change counts, pending clear bits and registration are atomics read without a lock. The pending queue is lock-free. Writing to a block that's dirty already takes no State lock.
- **No allocation on hot paths:** Multi-block sections hold their guards in fixed arrays, served flagging keeps its path inline, and chain logs exist only for directories a listing watches.
- **Compile-time variants:** Tiers and other variants are generic parameters or constants, so shared bodies pay no runtime branch and their futures hold only their own state.
- **Small futures:** Every future that awaits a node operation contains it, so cold paths (loading from the store, older formats, releasing a slot after an error) are boxed, entry points avoid holding their arguments twice, and tests assert a size budget for the futures of block lookups.
- **Synchronous fast paths:** Most writers try a synchronous path on resident blocks before building any async state; flagging a directory that's dirty already, on a served State, costs two read locks, the State's and the block's.
- **Sweeping only what may hold a slot:** Flagging a block where it may hold a detached slot costs one store under a lock its section holds already, and sweeping reads only the flagged blocks in memory. A session that only reads never sweeps.

The release profile keeps debug assertions, so checks wanted only under test sit behind the `state-hooks` feature rather than in `debug_assert!`.

### Measurement

A change can cost instructions without failing any test, so the node operations are measured as well as tested. Two benchmarks drive them the way their callers do: `state_bench` in `lore-revision/examples/` for the served, swap, discard and `Unserved` workloads, and `revision_tree_bench` in `lore/examples/` for the C API's verbs. Both refuse to build with `state-hooks`. Under `perf stat` they count instructions across each workload's operations alone, and a change to the module is compared workload by workload against a baseline revision. `state_bench --sizes` reports the sizes of the main records, tests assert size budgets for the futures of block lookups, and `lore-revision/tests/fuse_allocations.rs` holds the per-request allocation budgets of the VFS provider for Linux.

### Trade-offs

Each of these buys speed or simplicity on the common path at a cost elsewhere:

- **Retries instead of held locks:** An operation never holds a lock across a walk or an await. Under heavy churn in one directory it can answer `SlowDown`.
- **The served rule's climb:** The first served change below a chain of clean directories flags every clean level above it, one pair lock per level.
- **Detached slots until sweeping:** A detached record keeps its slot and its name until sweeping frees it, so a served State holds them until its provider sweeps, and staging and committing pay a scan of each flagged block in memory.
- **Freed slots in full blocks:** A full block that frees a slot stays off the unused-block chain until the State next saves.
- **Logs with a horizon:** A chain log holds a bounded window of changes (`CHAIN_LOG_MAX`), and a listing older than the window rebuilds itself. A log keeps the largest window it grew to while its block is in memory, at most 24 KiB for each directory a listing watched, even once no listing watches it or its slot is freed.

## How correctness is checked

A race shows only in rare interleavings, and a broken tree raises no error, so correctness is checked at several levels:

- **The write boundary:** The compiler checks it: a write outside the State doesn't compile.
- **Semantics:** Each family of operations is tested without hooks in `lore-revision/tests/unit/state/`.
- **Scheduling points:** The `state-hooks` feature, which no build turns on unless it asks, as the stress harness, the deterministic scenarios and the stress suite's run of lore-revision's unit tests do, adds a scheduling point wherever an operation holds no lock between sections, and records which locks each step reads or writes. With no scheduler installed a point costs one relaxed load. Under the feature the module also asserts its lock order and, where a scheduler is installed, that no task reaches a scheduling point holding a block guard.
- **Deterministic exploration:** Each deterministic scenario runs its operations as tasks on one thread, one step at a time, and explores every order of their steps, or every order up to a preemption bound, pruning orders that only swap independent steps. Scenarios cover the hazards the module closes, the served chain rule against the operations that race it, the C API's verbs, settling on a served State, and loads that fail part way.
- **Invariants:** A checker reads a quiescent State through its readers and the accessors a `test-util` build adds: the tree's shape, the slots, the dirty state and the names.
- **The log:** Under `state-hooks`, every write section records what it did (publishing, detaching, moving, taking or freeing a slot, refusing, and each node's changed words) in an order that's a valid replay order. So that a listing or a children iterator can be judged against the chain it read, each one, as it starts on a directory, also records the change count it read with the directory's first child, under that block's read lock. A model replays the log in that order, judges each operation's outcome at its place in it and each section's writes, and holds the State a run leaves to what it replayed.
- **Stress:** The stress suite in `scripts/state-stress/` runs mixes of operations, each mostly of one tier's callers and more at once than production runs them, on many threads that yield at random scheduling points, again and again on random settings, and checks every run through the model and the invariants. It also runs the deterministic scenarios in loops, so their explorations meet a loaded machine.

## Source pointers

- `lore-revision/src/state/block.rs` — `Node`, `NodeBlock` and `NodeBlockWriter`: records, blocks, the block lock, in-memory block state and chain logs; re-exported as `lore_revision::node`.
- `lore-revision/src/state.rs` — `State::block`, `State::block_modified` and `State::serialize`: residency, registration and saving; `ChainWalk` and `State::find_subnode`: walks and lookups.
- `lore-revision/src/state/node/mod.rs` — the contract, as numbered clauses C1 to C15, each a guarantee the node operations keep; the races it closes, numbered H1 to H25; and `Tier`, the tiers. The code and the tests cite clauses and hazards by these numbers.
- `lore-revision/src/state/node/locks.rs` — `BlockLocks`, `PairLock` and the served chain rule's test.
- `lore-revision/src/state/node/attach.rs`, `detach.rs` and `relocate.rs` — attaching, building copies, detaching, moving and renaming.
- `lore-revision/src/state/node/rewrite.rs` and `in_place.rs` — content and in-place changes.
- `lore-revision/src/state/node/mark.rs` and `clear.rs` — flagging, clearing and settling.
- `lore-revision/src/state/node/discard.rs`, `pending.rs` and `slots.rs` — freeing, the pending queue and slot allocation.
- `lore-revision/src/state/node/sweep.rs` — sweeping detached slots, unserved and served.
- `lore-revision/src/state/node/hooks.rs` and `hooks/log.rs` — scheduling points and the log.
- `lore-revision/src/fs/fuse/listing.rs` — the VFS listing that replays chain logs and shows names absent, in the provider for Linux.
- `lore-revision/examples/state_stress/` — the stress harness: the deterministic and chaos schedulers, the log's collector, the model, the invariants and the mixes of operations.
- `lore-revision/tests/state_scenarios/` — the deterministic scenarios, which share the stress harness, and `lore/tests/state_scenarios/` those that run the C API's verbs.
- `scripts/state-stress/` — the stress suite's driver and its README.

## See also

- [ADR-00028](../decisions/00028-state-node-operations-behind-one-write-boundary.md) — the decision to change trees only through the node operations, to free detached slots by sweeping, and the stress suite behind them.
- [Parallel path staging](parallel-path-staging.md) — how staging adds each parent and name only once.
- [System design](../../explanation/system-design.md) — the stored layout of the revision tree and dirty tracking.
