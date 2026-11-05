# State node stress suite

A stress suite for the node operations of a `State` (`lore-revision/src/state/node/`), the code
that changes a revision tree's nodes, their chains and their flags, run by hand when the node
operations change, not in CI. It runs on Linux.

Many tasks change one tree at once: a VFS provider's requests beside its settles and its swaps
to other revisions, the C API's verbs on one handle, staging's parallel walks. The operations keep
the tree whole under a written contract, in the module documentation of
`lore-revision/src/state/node/mod.rs`. The suite runs them concurrently and checks the outcome of
every one against that contract. It looks for four things:

- **Lost or corrupt structure.** A node in no chain or in two, a chain that doesn't end, a slot
  freed while a node uses it, a slot left detached that no sweep would free, a name that doesn't
  match its hash, a dirty node below a clean directory where the contract keeps none, a clean
  served directory holding other children than its committed directory.
- **Wrong answers.** An operation that succeeded where the tree as it stood made it impossible,
  refused where nothing could refuse it, or answered a reader with something the directory never
  held.
- **Writes outside their section.** A field of a node changed by a section that doesn't own it.
- **Hangs.** An operation that never ends, as a lock taken out of order leaves it.

## Parts

- `lore-revision/examples/state_stress/` is the harness. It builds only with lore-revision's
  `state-stress` feature, which turns on the node operations' scheduling points (`state-hooks`)
  and `test-util`, and which `stress.py build` enables, so `cargo test` never builds it:
  - `chaos` runs the harness's mixes over and over until its time or its run count is up. Each
    run builds its mix's tree, takes a random number of rounds of the mix's operations, one
    task each, in a random order, and runs them all at once on a runtime of a random number of
    workers, each yielding at random scheduling points and stalling at random between steps no
    await separates, so the windows between an operation's steps open wide enough for other
    threads to run in them. Once they end, with nothing running, the run reclaims the slots its
    operations left detached or in flight, as a stage or a commit sweeps them before it saves
    and a VFS provider where it pauses its requests: a served tree keeps a third of them at
    random, as the kernel would still reference them. Every write section records what it did to
    a log. Then the model replays the log, judges each operation's outcome at its place in it and
    each section's writes, and checks the State against what the model holds and against the
    contract's invariants: the tree's shape, the slots, the dirty state and the names. An
    operation the mix leaves nothing to refuse must succeed, so a wrong refusal fails the run
    too. Each run takes a variant: `plain` runs every operation to its end; `drop` aborts some of
    those whose callers may drop them, wherever they stand, as a caller dropping its future does;
    `fault` builds the tree over a store that fails reads on demand and fails one read in six
    after the first burst. Both run the operations in two bursts, each reclaimed, replayed and
    checked as above, with a save and an eviction of the clean blocks between them, so the
    second burst loads what it reads. `fault` runs on the unserved mixes alone: a served tree is
    never saved, so its blocks stay dirty in memory and no read after the first burst could
    fail. The deterministic scenarios fail the served operations' loads instead.
  - `list` names the mixes; with `--ops`, each mix's operations.
  - `harness/` holds what the harness and the deterministic tests share: the deterministic
    scheduler, the chaos scheduler, the log's collector, the model, the invariants' checker, the
    catalogue of operations and the mixes.
- `lore-revision/tests/state_scenarios/` holds the deterministic scenarios, and
  `lore/tests/state_scenarios/` those that run the C API's verbs. They step the operations one task
  at a time through their scheduling points and explore every order of their steps, or every order
  up to a bound, canceling operations at every point a caller can drop them, and run each mix in
  full on fixed seeds, in each variant. They include the harness by its path, and build only with
  their package's `state-stress` feature, so a plain `cargo test` runs neither and tests the build
  that ships.
- `stress.py` builds the harness and the suites and runs campaigns of them.

Each mix runs its tier's kinds together, more at once than production runs them. Three go past
the tiers: `exclusive` runs `lore dirty`, the scan and the commit's and the reset's clears at once,
where production runs one command per exclusive claim; `listing` runs the stage's marked listings
beside served writers; and the renames of `exclusive` change a name's hash beside other writers,
which no caller does.

| Mix | What runs together |
| --- | --- |
| `exclusive` | The changes under an exclusive claim, as staging, committing and the filesystem scan make them: checked creates, adds, a scan's directory chains, adds discarded, renames, marks, clears, rewrites, in-place and owned changes, links pinned again, listings through a link and collections of dirty paths through it. |
| `exclusive-across-blocks` | The same, with the discards' directories in one node block and the nodes added into them in the next. |
| `shared` | The C API's verbs under a shared claim: adds, modifies, deletes and their restores, moves after the batch plan's walk or walking for themselves, deletes of added subtrees queued pending, a batch delete of tracked and added subtrees spread over futures of its own beside adds into them, beside has-children reads and collections of dirty paths; then the pending queue drained alone. |
| `served-settle` | VFS requests on a served tree: creates, removals, renames over a committed file, renames of files and of directories holding local content, renames refused a directory holding none, marks, copy-ups, creates and a rename contending for one name, RMDIRs, a rename leaving its directory clean children other than the committed ones; beside a settle of a directory they change, then settles of the directories they move none of, one of which must clear. |
| `served-swap-copy`, `served-swap-detach`, `served-swap-rewrite`, `served-flip` | The same requests, beside a swap to another revision copying a directory in, detaching a clean directory or rewriting a clean file, or beside a commit turning a dirty file clean. |
| `listing` | Walks, listings by the VFS and by the stage, reads since a marked listing, has-children reads, collections of dirty paths, name and path reads, and creates and renames through a listing's proof of absence, beside served changes of the directories they read. |

## Running

From the repository root:

```sh
python3 scripts/state-stress/stress.py build              # the harness, and the suites' tests
python3 scripts/state-stress/stress.py scenarios          # the deterministic suites, once
python3 scripts/state-stress/stress.py chaos --minutes 30
python3 scripts/state-stress/stress.py soak --minutes 30  # the deterministic suites, in loops
python3 scripts/state-stress/stress.py all --minutes 20   # chaos and soak 20 minutes each, scenarios once
```

The modes:

| Mode | What it does |
| --- | --- |
| `chaos` | Processes of `state_stress chaos`, `--jobs` at a time, each for `--chunk` seconds on a seed of its own drawn from the campaign's, over every mix or those `--mix` names. Each process cycles through its mixes, every run on random rounds, a random order, a random number of workers up to twice the cores, a random share of scheduling points that yield, and a random variant of those `--variant` names that the mix runs. |
| `scenarios` | lore-revision's scenarios, the C API's, then lore-revision's unit tests built with the scheduling points, once each. |
| `soak` | `scenarios` again and again until the time is up, so the explorations' runs meet a machine under their own load. |

Useful options:

- `--minutes` is the time budget per mode.
- `--seed` replays a campaign's draws of the chaos processes' seeds; the summary records it. A
  chaos run itself doesn't replay: its interleaving is the threads'.
- `--jobs` is how many chaos processes run at once, by default one per eight cores.
- `--variant` names the variants chaos runs draw from: `all`, the default, or `plain`, `drop`
  and `fault` joined by commas. A mix that runs none of them is left out.
- `--stop-on-new` stops a `chaos` campaign at the first violation class not listed in `KNOWN` in
  `stress.py`: at once while `KNOWN` is empty, as each process then stops at its first violation,
  and otherwise when the process that met it ends its chunk, so a known class doesn't cut every
  process short.
- `--hang-seconds` is how long a chaos process may go with no run ending before it counts itself
  hung; `--suite-timeout` how long a deterministic suite may run.

The harness builds in the `release` profile, which aborts on a panic, as `store_stress` does. A
panic inside a run is a finding, whatever panicked: the driver counts each `panicked at` line as a
violation of class `panic: <where it panicked>`, as store-stress's driver does, an abort with no
such line as `<mix>-abort`, and a hang as `<mix>-hang`, each noting the run it ended from the last
`NOTE running` line before it. A chaos process still running twice `--hang-seconds` past its time,
and a suite past `--suite-timeout`, is killed with everything it started and counted as a hang.

`report [<results>]` merges every campaign under a results directory: violations and harness
failures with the campaigns that found them, coverage summed over all of them, and the gaps none
covered. It exits non-zero when any campaign found a violation or a harness failure: a process
that exited other than as its run allows, or a suite that failed with no test failing, as a crash
leaves.

Results go to `target/state-stress/results/<mode>-<time>-<pid>/`:

- one log per process, with its command line;
- `summary.txt`, with every violation class and the logs that found it, marked `NEW` or `known`,
  the harness failures, the coverage counters summed over the campaign, and the coverage gaps.

## The harness's output

`state_stress`'s lines follow `store_stress`'s conventions, `NOTE`, `COVERAGE`, `COVERAGE-GAP`,
`VIOLATION` and `HANG`; the `RUN` lines and the `REPORT` summary are its own:

- on stdout, `NOTE seed <seed>`; before each run, `NOTE running <run>`, its mix, variant, rounds,
  workers, share of yielding points and seed; after it, `RUN <run> ok`, or ending with its failure's
  class; at the end, `COVERAGE mix <mix> <runs> <passed>`, `COVERAGE variant <variant> <runs>
  <passed>`, `COVERAGE outcome dropped <count>` and `COVERAGE outcome failed <count>` for the calls
  the passing runs dropped and failed, `COVERAGE record <mix> <record> <count>` for
  the records the passing runs made, and `COVERAGE holder <mix> <record> <count>` for those of a
  served mix's holder; `COVERAGE call <kind> <count>` for the operations' calls; `COVERAGE arm
  <arm> <count>` for the branches races sent operations down; a `COVERAGE-GAP record <mix>
  <record>`, or `COVERAGE-GAP holder <mix> <record>` for a swap's or a flip's mix, for each record
  a mix exists to make that none of its passing runs made; and `REPORT runs <n> failed <n> yields
  <n> contentions <n>`;
- on stderr, `VIOLATION <mix>-<class>: <detail> (<run>)` for each finding of a failed run, its
  class one of `log` (a record or a point with no task, or one pushed into a full buffer), `calls`
  (a record held by no operation's call, or by two), `model` (the model's replay of a burst) and
  `checkpoint` (the State at a burst's end); and `HANG: ...`. An operation that answers what its
  mix leaves it no room to panics, as do the log's own checks, which aborts the process; a build
  that unwinds, as the tests do, reports the operation's as class `op` instead.

Exit codes: 0 clean, 1 violations, 2 usage, an unknown option included, 99 stopped at the first
violation (`--stop-on-violation`), 100 a hang: a task that didn't end within a minute, or no run
ending for `--hang-seconds`. A panic aborts the process.

A single run by hand:

```sh
B=target/release/examples/state_stress
$B list --ops
$B chaos --seconds 60
$B chaos --mix served-settle,listing --runs 20 --workers 8 --every 2 --seed 7
$B chaos --mix served-settle --variant drop --rounds 2 --workers 8 --every 2 --run-seed 1234
```

The last runs one run again from the settings its `NOTE running` line names: its operations'
order, its threads' yields and the drops or faults its variant draws, though not its interleaving,
which is the threads'.

## Known defects

`KNOWN` in `stress.py` maps the violation classes of a defect being worked on to it, so that they
don't drown new findings; the driver labels those classes `known`.
