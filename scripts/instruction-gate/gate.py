#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""The instruction gate: an A/B of two checkouts by the user-mode instructions every bench
workload spends per operation, counted on a hardware counter.

Wall-clock time on a shared machine varies by more than the regressions worth catching; a count of
retired instructions repeats to within 0.25%. Each workload is built alone, so the code around it
cannot change how it compiles, and counted in each checkout on one core. A workload passes when
the change spends at most 0.5% more net instructions per operation than the base.

Run by hand, on Linux; see README.md beside this file. Exits 0 when every workload passes, 1 when
one fails, and 2 when the gate cannot run: no counter, a failed build or a failed bench.
"""

import argparse
import hashlib
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field, replace
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

# The most a workload's net instructions per operation may grow over the base's.
LIMIT = 0.005

# The workloads that measure the timing loop alone and the loop with one empty bracket per
# operation: what every other count carries beside its operations, subtracted from it.
LOOP = "loop-null"
BRACKET = "bracket-null"


@dataclass(frozen=True)
class Bench:
    package: str
    features: str
    # Options every count passes.
    args: tuple[str, ...] = ()
    # Further counts of a workload's binary: workload to (label suffix, options) pairs.
    variants: dict[str, tuple[tuple[str, tuple[str, ...]], ...]] = field(default_factory=dict)
    # Whether only the thread that drives the bench counts, rather than every thread of it.
    main_thread: bool = False
    # Counts of each workload per checkout, alternating between them; the gate takes the medians.
    runs: int = 1


BENCHES = {
    "state_bench": Bench(
        package="lore-revision",
        features="state-bench",
        # One writer on a current_thread runtime, the only mode that leaves a workload's setup
        # between its operations out of the count.
        args=("--current-thread",),
        variants={
            # On a State read back from the store, so the blocks load on demand.
            "discard-files": (("cold", ("--cold",)),),
            "discard-dirs": (("cold", ("--cold",)),),
            "discard-dirs-spread": (("cold", ("--cold",)),),
            # Each depth of settled directories the unlink's marks climb through.
            "served-unlink-settled-deep": tuple((str(d), ("--depth", str(d))) for d in range(2, 9)),
        },
    ),
    # Every C API call hands its events to a forwarder on Lore's multi-threaded core runtime. How
    # much work stealing and parking its threads do varies from run to run by 1% of the count and
    # more, past the gate's margin; the verbs and the State's operations run on the bench's own
    # thread. Its count still varies from run to run, by up to 2% in some workloads, hence the
    # median of several.
    "revision_tree_bench": Bench(package="lore", features="revision-tree-bench", main_thread=True, runs=5),
}


class GateError(Exception):
    """The gate cannot run, or cannot finish."""


@dataclass(frozen=True)
class Count:
    instructions: int
    ops: int
    brackets: int


def net(count, loop, bracket):
    """`count`'s instructions per operation, less the timing loop's cost per operation and the
    cost of each bracket it made, measured by the loop and bracket workloads of its checkout."""
    per_bracket = (bracket.instructions - loop.instructions) / bracket.brackets if bracket.brackets else 0.0
    return (count.instructions - count.brackets * per_bracket) / count.ops - loop.instructions / loop.ops


def cpu_list(text):
    """The CPUs a sysfs list such as `0-11,14` names."""
    cpus = set()
    for part in text.strip().split(","):
        if part:
            first, _, last = part.partition("-")
            cpus.update(range(int(first), int(last or first) + 1))
    return cpus


def counter_core(requested):
    """The core to count on and the perf event counting its user-mode instructions. Hybrid Intel
    parts count on a performance core, whose PMU is `cpu_core`; elsewhere one PMU serves every
    core."""
    hybrid = Path("/sys/devices/cpu_core/cpus")
    if hybrid.exists():
        cores, event = cpu_list(hybrid.read_text()), "cpu_core/instructions/u"
    else:
        cores, event = cpu_list(Path("/sys/devices/system/cpu/online").read_text()), "instructions:u"
    usable = sorted(cores & os.sched_getaffinity(0))
    if requested is not None:
        if requested not in usable:
            raise GateError(f"CPU {requested} is not one this process may run on that counts with {event}: "
                            f"{usable}")
        return requested, event
    if not usable:
        raise GateError(f"no CPU this process may run on counts with {event}")
    # The highest-numbered, away from CPU 0, which takes most of the interrupts.
    return usable[-1], event


def perf_counts(path):
    """The counts in a `perf stat -x, -o` file, one (count, percentage running) pair per event
    line; the count is None where perf could not count."""
    rows = []
    for line in Path(path).read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split(",")
        if len(fields) < 5:
            continue
        value = int(fields[0]) if fields[0].isdigit() else None
        try:
            running = float(fields[4])
        except ValueError:
            running = 0.0
        rows.append((value, running))
    return rows


def probe(cpu, event, scratch):
    """Refuse to run where no PMU counts user-mode instructions on `cpu`: no perf, a kernel that
    forbids counting, or no counter, as in most virtual machines."""
    for tool in ("perf", "taskset", "cargo"):
        if shutil.which(tool) is None:
            raise GateError(f"{tool} is not installed")
    out = scratch / "probe.csv"
    result = subprocess.run(["perf", "stat", "-e", event, "-x,", "-o", str(out), "--", "taskset", "-c", str(cpu),
                             "true"], capture_output=True, text=True)
    counted = [value for value, _ in perf_counts(out) if value] if out.exists() else []
    if result.returncode != 0 or not counted:
        detail = result.stderr.strip() or (out.read_text().strip() if out.exists() else "no output")
        raise GateError(f"no PMU counts user-mode instructions here: perf stat -e {event} on CPU {cpu} counted "
                        f"nothing.\n{detail}\nThe gate needs a hardware counter; most virtual machines have none.")


def target_dir(work, checkout):
    """The checkout's target directory, which every gate run against it shares, so later runs
    rebuild only what changed."""
    digest = hashlib.sha256(str(checkout).encode()).hexdigest()[:8]
    return work / "build" / f"{checkout.name}-{digest}"


class Side:
    """One checkout of the A/B: its builds and its counts, per bench."""

    def __init__(self, label, checkout, work, logs):
        self.label = label
        self.checkout = checkout
        self.target = target_dir(work, checkout)
        self.logs = logs
        self.counts: dict[tuple[str, str], Count] = {}

    def build(self, bench_name, workload=None):
        """Build the bench, with only `workload` in it where given, and return the binary. One
        codegen unit and no incremental compilation: with the release profile's many units, a
        change to unrelated code repartitions them and moves a workload's count by up to 8%."""
        bench = BENCHES[bench_name]
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.target), CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1",
                   CARGO_PROFILE_RELEASE_INCREMENTAL="false")
        # Read at build time by the benches: every other workload's code is left out.
        env.pop("STATE_BENCH_ONLY", None)
        if workload:
            env["STATE_BENCH_ONLY"] = workload
        command = ["cargo", "build", "--release", "-p", bench.package, "--example", bench_name, "--features",
                   bench.features]
        log = self.logs / f"{bench_name}-{self.label}-{workload or 'all'}.build.log"
        with open(log, "w") as output:
            output.write(" ".join(command) + "\n")
            output.flush()
            result = subprocess.run(command, cwd=self.checkout, env=env, stdout=output, stderr=subprocess.STDOUT)
        if result.returncode != 0:
            raise GateError(f"the {self.label} build of {bench_name} {workload or ''} failed (exit "
                            f"{result.returncode}); see {log}")
        return self.target / "release" / "examples" / bench_name

    def workloads(self, bench_name, binary):
        """The workloads the bench names, from its `--list`."""
        result = subprocess.run([str(binary), "--list"], capture_output=True, text=True)
        if result.returncode != 0:
            raise GateError(f"the {self.label} {bench_name} names no workloads:\n{result.stderr}")
        return result.stdout.split()


def build_sides(sides, bench_name, workload=None):
    """Build the bench in each side at once, each in its own target directory, and return each
    side's binary. A build on other cores does not change a count, so only the counts run alone."""
    with ThreadPoolExecutor(max_workers=len(sides) or 1) as pool:
        builds = {side: pool.submit(side.build, bench_name, workload) for side in sides}
    return {side: build.result() for side, build in builds.items()}


class Counter:
    """`perf stat` of one bench run pinned to one core, its count enabled only while the bench
    says, through the control FIFOs the benches take as `--perf-ctl` and `--perf-ack`."""

    def __init__(self, cpu, event, scratch, ops):
        self.cpu = cpu
        self.event = event
        self.ops = ops
        self.ctl = scratch / "ctl"
        self.ack = scratch / "ack"
        self.stat = scratch / "stat.csv"
        os.mkfifo(self.ctl)
        os.mkfifo(self.ack)

    def count(self, binary, args, log, main_thread=False):
        """Count one run of `binary`, every thread of it or with `main_thread` only the one it
        starts on, which the threads it spawns then leave out of the count."""
        command = ["perf", "stat", "-e", self.event, "-x,", "-o", str(self.stat), *(["-i"] if main_thread else []),
                   f"--control=fifo:{self.ctl},{self.ack}", "-D", "-1", "--",
                   "taskset", "-c", str(self.cpu), str(binary), "--ops", str(self.ops), *args,
                   "--perf-ctl", str(self.ctl), "--perf-ack", str(self.ack)]
        self.stat.unlink(missing_ok=True)
        result = subprocess.run(command, capture_output=True, text=True)
        stat = self.stat.read_text() if self.stat.exists() else ""
        log.write_text(" ".join(command) + "\n" + result.stdout + result.stderr + stat)
        if result.returncode != 0:
            raise GateError(f"the bench failed (exit {result.returncode}); see {log}")
        lines = [line for line in result.stdout.splitlines() if line.startswith("BENCH ")]
        if len(lines) != 1:
            raise GateError(f"the bench printed {len(lines)} BENCH lines, not one; see {log}")
        ops = re.search(r"\bops=(\d+)", lines[0])
        brackets = re.search(r"\bbrackets=(\d+)", lines[0])
        if not ops or not brackets or int(ops.group(1)) == 0:
            raise GateError(f"the bench reported no operations or no brackets; see {log}")
        counts = perf_counts(self.stat)
        counted = [value for value, _ in counts if value is not None]
        if not counted:
            raise GateError(f"perf counted nothing; see {log}")
        # A counter that ran for part of the time it was enabled has been scaled up from a sample.
        if any(value is not None and running < 100.0 for value, running in counts):
            raise GateError(f"the counter ran for only part of the count, which perf scaled; see {log}")
        return Count(sum(counted), int(ops.group(1)), int(brackets.group(1)))


def verdict(base, change, noise):
    """Whether the change's net count passes against the base's, whether a failing one is above it
    by no more than `noise`, a share of the base's (None from a single count), and the verdict's
    text."""
    ok = change <= base + abs(base) * LIMIT
    within = not ok and noise is not None and change - base <= abs(base) * noise
    return ok, within, "ok" if ok else "FAIL, within noise" if within else "FAIL"


def run(args):
    base_checkout = Path(args.base).resolve()
    change_checkout = Path(args.change).resolve()
    for checkout in (base_checkout, change_checkout):
        if not (checkout / "Cargo.toml").is_file():
            raise GateError(f"{checkout} is not a checkout: it has no Cargo.toml")
    cpu, event = counter_core(args.cpu)
    scratch = Path(tempfile.mkdtemp(prefix="instruction-gate-"))
    try:
        probe(cpu, event, scratch)
        counter = Counter(cpu, event, scratch, args.ops)
        work = Path(args.work).resolve()
        results = work / "results" / f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
        logs = results / "logs"
        logs.mkdir(parents=True)
        binaries = results / "bin"
        if args.keep_binaries:
            binaries.mkdir()
        print(f"base {base_checkout}\nchange {change_checkout}\ncounting {event} on CPU {cpu}, {args.ops} "
              f"operations per workload\nresults {results}", flush=True)
        sides = [Side("base", base_checkout, work, logs), Side("change", change_checkout, work, logs)]
        # Every bench's workloads first, so a misnamed --only fails before the long part.
        listed = {}
        for bench_name in args.bench:
            print(f"building {bench_name}", file=sys.stderr, flush=True)
            for side, binary in build_sides(sides, bench_name).items():
                names = side.workloads(bench_name, binary)
                if names[:2] != [LOOP, BRACKET]:
                    raise GateError(f"the {side.label} {bench_name} does not begin with {LOOP} and {BRACKET}")
                listed[(bench_name, side.label)] = names
        unknown = (args.only or set()) - {name for names in listed.values() for name in names}
        if unknown:
            raise GateError(f"no bench has the workloads {', '.join(sorted(unknown))}")
        failed = []
        for bench_name in args.bench:
            names = (listed[(bench_name, "base")], listed[(bench_name, "change")])
            failed += gate_bench(bench_name, names, sides, counter, args, binaries, results)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    if failed:
        print(f"\nFAILED: {', '.join(failed)}")
        return 1
    print("\nevery workload passes")
    return 0


def gate_bench(bench_name, names, sides, counter, args, binaries, results):
    """Build, count and gate every workload of one bench, given the names each side's bench
    lists; returns the labels that failed."""
    bench = BENCHES[bench_name]
    runs = args.runs or bench.runs
    base_names, change_names = names
    order = change_names + [name for name in base_names if name not in change_names]
    if args.only:
        order = [name for name in order if name in (LOOP, BRACKET) or name in args.only]
    print(f"\n{bench_name}, the median of {runs} count{'s' if runs > 1 else ''} per checkout\n"
          f"{'workload':34} {'base':>12} {'change':>12} {'delta':>10} {'pct':>8} {'noise':>7}  verdict", flush=True)
    by_label = {side.label: side for side in sides}
    failed = []
    csv = {side.label: open(results / f"{bench_name}-{side.label}.csv", "w") for side in sides}
    try:
        for output in csv.values():
            output.write("workload,run,instructions,ops,brackets,net_per_op\n")
        for workload in order:
            present = [side for side, side_names in zip(sides, names, strict=True) if workload in side_names]
            print(f"building {bench_name} {workload}", file=sys.stderr, flush=True)
            built = build_sides(present, bench_name, workload)
            if args.keep_binaries:
                for side, binary in built.items():
                    shutil.copy2(binary, binaries / f"{bench_name}-{side.label}-{workload}")
            labels = [(workload, ())] + [(f"{workload}-{suffix}", extra)
                                         for suffix, extra in bench.variants.get(workload, ())]
            counts: dict[tuple[str, str], list[Count]] = {}
            for run in range(runs):
                for side, binary in built.items():
                    for label, extra in labels:
                        log = side.logs / f"{bench_name}-{side.label}-{label}-{run}.count.log"
                        count = counter.count(binary, (*bench.args, *extra), log, bench.main_thread)
                        counts.setdefault((side.label, label), []).append(count)
            for (side_label, label), side_counts in counts.items():
                median = statistics.median_low(count.instructions for count in side_counts)
                by_label[side_label].counts[(bench_name, label)] = replace(side_counts[0], instructions=median)
            nets, noise = {}, {}
            for (side_label, label), side_counts in counts.items():
                side = by_label[side_label]
                loop = side.counts[(bench_name, LOOP)]
                bracket = side.counts.get((bench_name, BRACKET), side.counts[(bench_name, label)])
                per_run = [net(count, loop, bracket) for count in side_counts]
                for run, (count, per_op) in enumerate(zip(side_counts, per_run, strict=True)):
                    csv[side_label].write(f"{label},{run},{count.instructions},{count.ops},{count.brackets},"
                                          f"{per_op:.1f}\n")
                csv[side_label].flush()
                nets[(side_label, label)] = statistics.median(per_run)
                noise[(side_label, label)] = (max(per_run) - min(per_run)) / abs(nets[(side_label, label)] or 1)
            if workload in (LOOP, BRACKET):
                continue
            for label, _ in labels:
                base, change = nets.get(("base", label)), nets.get(("change", label))
                if base is None:
                    print(f"{label:34} {'new':>12} {change:12.0f}", flush=True)
                    continue
                if change is None:
                    print(f"{label:34} {base:12.0f} {'missing':>12}  FAIL", flush=True)
                    failed.append(label)
                    continue
                spread = max(noise[("base", label)], noise[("change", label)]) if runs > 1 else None
                ok, within, text = verdict(base, change, spread)
                delta = round(change - base)
                pct = round(100.0 * (change - base) / base, 2) if base > 0 else 0.0
                shown = f"{100.0 * spread:.2f}%" if spread is not None else "-"
                print(f"{label:34} {base:12.0f} {change:12.0f} {delta:+10d} {pct + 0.0:+7.2f}% {shown:>7}  {text}",
                      flush=True)
                if not ok:
                    failed.append(f"{label} (within noise)" if within else label)
    finally:
        for output in csv.values():
            output.close()
    return failed


def bench_list(value):
    names = [name for name in value.split(",") if name]
    for name in names:
        if name not in BENCHES:
            raise argparse.ArgumentTypeError(f"no bench {name}: {', '.join(BENCHES)}")
    return names


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError(f"{value} is not above 0")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("base", help="the checkout to compare against: a Lore instance or any directory of the "
                                     "sources")
    parser.add_argument("change", help="the checkout under test")
    parser.add_argument("--bench", type=bench_list, default=list(BENCHES),
                        help=f"the benches to gate, comma-separated (default {','.join(BENCHES)})")
    parser.add_argument("--only", type=lambda value: set(value.split(",")),
                        help="gate only these workloads, comma-separated, with their variants")
    parser.add_argument("--ops", type=positive, default=5000, help="operations per workload (default 5000)")
    parser.add_argument("--runs", type=positive, help="counts of each workload per checkout, whose medians the gate "
                                                      "compares (default 1 for state_bench, 5 for "
                                                      "revision_tree_bench)")
    parser.add_argument("--cpu", type=int, help="the core to count on (default: the highest-numbered that "
                                                "counts, a performance core on hybrid Intel parts)")
    parser.add_argument("--work", default=REPO / "target" / "instruction-gate",
                        help="where the builds and the results go (default target/instruction-gate of the "
                             "checkout holding this script)")
    parser.add_argument("--keep-binaries", action="store_true",
                        help="keep each workload's binaries in the results, for perf record and perf annotate")
    args = parser.parse_args()
    try:
        return run(args)
    except GateError as error:
        print(f"instruction gate: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
