# Instruction gate

An A/B of two checkouts by the instructions each bench workload spends per operation, run by hand
before a change to a hot path lands, not in CI.

Wall-clock time on a shared machine moves by more than the regressions worth catching. A count of
the user-mode instructions a thread retires in a workload repeats from run to run, to within 0.25%
in most workloads, so a change that adds a few hundred instructions to an operation of tens of
thousands shows. The gate counts them on a hardware counter, which is why it runs on a physical
Linux machine.

## What it does

`gate.py <base> <change>` takes two checkouts of the repository, two Lore instances or any two
directories of the sources, and gates every workload of two benches:

- `state_bench` (`lore-revision/examples/state_bench`), the State's node operations;
- `revision_tree_bench` (`lore/examples/revision_tree_bench`), the C API's revision-tree verbs.

For each workload, in each checkout, it:

1. Builds the bench with that workload alone (`STATE_BENCH_ONLY`), in one codegen unit and with
   no incremental compilation. The release profile's many units repartition when unrelated code
   changes, which moved counts by up to 8%; built alone in one unit, a workload's count moves only
   when code it runs or inlines changes.
2. Runs it pinned to one core under `perf stat`, which counts only while the bench says, through
   the control FIFOs the benches take as `--perf-ctl` and `--perf-ack`: across the timed operations,
   with any setup between them left out. Each piece of setup left out is a bracket.
3. Takes the net count per operation: the count, less what the timing loop and each bracket still
   cost, measured by the `loop-null` and `bracket-null` workloads of the same checkout.

`state_bench` counts every thread of the bench, once per checkout: its counts repeat to within
0.1%. `revision_tree_bench` counts only the thread that drives it, where the verbs and the State's
operations run. Every C API call also hands its events to a forwarder on Lore's multi-threaded core
runtime, and how much work stealing and parking those threads do varies from run to run by 1% of
the count and more, past the gate's margin. Work a change moves onto another thread of
`revision_tree_bench` therefore leaves its count. The bench's own thread still varies from run to
run, by 0.1% in some workloads and by up to 2% in others (`capi-add`, `capi-delete-tracked`), so
the gate counts each of its workloads five times per checkout, alternating between the two, and
compares the medians.

A workload passes when the change's net count per operation is at most 0.5% above the base's.

The workloads come from each bench's `--list`, so a new workload is gated as soon as it's in the
bench. A workload only the change has prints as `new`; one only the base has fails as `missing`.
Some workloads are also counted with extra options, as variants: the discard workloads on a State
read back from the store (`-cold`), and `served-unlink-settled-deep` at depths 2 to 8 (`-<depth>`).

## Requirements

- Linux with `perf` and `taskset`, and `cargo`.
- A performance counter that counts user-mode instructions, open to the user:
  `kernel.perf_event_paranoid` at 2 or below. On hybrid Intel parts the gate counts on a performance
  core, through the `cpu_core` PMU; on AMD and other parts with one kind of core, through the
  generic `instructions` event. The core is the highest-numbered one the gate may run on, unless
  `--cpu` names one.

The gate checks the counter before it builds anything and refuses to run where it counts nothing,
as in most virtual machines.

## Running

From either checkout:

```sh
python3 scripts/instruction-gate/gate.py <base> <change>
python3 scripts/instruction-gate/gate.py ../base . --bench revision_tree_bench --only capi-modify
```

| Option | Meaning |
| --- | --- |
| `--bench` | The benches to gate, comma-separated; both by default. |
| `--only` | Only these workloads, comma-separated, with their variants. |
| `--ops` | Operations per workload, 5000 by default. Some workloads cap it. |
| `--runs` | Counts of each workload per checkout, whose medians the gate compares; 1 for `state_bench` and 5 for `revision_tree_bench` by default. |
| `--cpu` | The core to count on. |
| `--work` | Where the builds and the results go, `target/instruction-gate` of the checkout holding the script by default. |
| `--keep-binaries` | Keep every workload's binaries in the results, for `perf record` and `perf annotate`. |

Each checkout builds in a target directory of its own under `<work>/build/`, which later runs
against it reuse, and the two checkouts build at once. The first build compiles every dependency
in one codegen unit, which takes minutes; set `CARGO_BUILD_JOBS` to build fewer crates at once
where memory is short. After it, each workload rebuilds only its bench, in about 20 seconds; a
whole A/B of both benches takes about 45 minutes on an i7-13800H. Load on other cores doesn't
change a count, but a checkout must not change while the gate builds from it.

## Output

One table per bench, a line per workload or variant as soon as both checkouts have counted it:
the base's and the change's net instructions per operation, the difference, the percentage, the
noise and the verdict. The noise is the larger of the two checkouts' spreads, from their lowest
count to their highest, as a share of the median. A workload that fails by a difference within it
still fails, as `FAIL, within noise`: more `--runs` tell whether it moved. The closing line lists
the workloads that failed, those within the noise marked so. The results go to
`<work>/results/<time>-<pid>/`:

- `<bench>-base.csv` and `<bench>-change.csv`: each count's instructions, operations, brackets and
  net instructions per operation;
- `logs/`: each build's output, and each count's command line, bench output and `perf stat` output;
- `bin/`, with `--keep-binaries`: each workload's binaries, named `<bench>-<side>-<workload>`.

Exit codes: 0 when every workload passes, 1 when one fails or is missing, 2 when the gate can't
run: no counter, a failed build or a failed bench.

## Finding what moved

A failing workload's binaries, kept with `--keep-binaries`, show where the instructions went. Run
each under `perf record` on the event and the core the gate printed, with the options its count
log shows: `state_bench` takes `--current-thread`, and `revision_tree_bench` counts with `-i`, its
own thread alone. Give it enough operations for a few thousand samples, since the kernel caps the
sample rate (`kernel.perf_event_max_sample_rate`), and compare the two profiles by symbol:

```sh
perf record -i -e cpu_core/instructions/u -o base.data -- taskset -c 11 bin/<bench>-base-<workload> --ops 100000
perf record -i -e cpu_core/instructions/u -o change.data -- taskset -c 11 bin/<bench>-change-<workload> --ops 100000
perf diff base.data change.data
perf annotate -i change.data <symbol>
```

These profiles cover setup too, which the gate leaves out. A difference spread over functions whose
source didn't change points at inlining or layout in the one codegen unit; one concentrated in
changed code points at the code.
