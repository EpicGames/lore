#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Drive the stress suite of the State's node operations: chaos campaigns of the harness's mixes,
each run on random settings and checked by the model and the checkpoint, and the deterministic
scenarios of lore-revision and of the C API, once or in loops.

Run by hand, on Linux; see README.md beside this file. Every campaign writes to
<results>/<mode>-<time>-<pid>/: one log per process with its command line, and summary.txt with
violations, harness failures and coverage, across every process.
"""

import argparse
import json
import os
import random
import re
import signal
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

# Classes the harness reports for defects already known, kept apart from new findings: class name
# to the defect. Empty while none is open.
KNOWN = {}

# The deterministic suites: lore-revision's scenarios and the C API's, and lore-revision's unit tests
# with the scheduling points built in.
SUITES = {
    "lore-revision-scenarios": ["cargo", "test", "--release", "-p", "lore-revision", "--features", "state-stress",
                                "--test", "state_scenarios"],
    "lore-scenarios": ["cargo", "test", "--release", "-p", "lore", "--features", "state-stress",
                       "--test", "state_scenarios"],
    "lore-revision": ["cargo", "test", "--release", "-p", "lore-revision", "--features", "state-stress",
                      "--test", "unit"],
}

# The variants a chaos run draws from, as the harness names them.
VARIANTS = ("plain", "drop", "fault")

# How a process that aborted exits: killed by SIGABRT, or 134 where a shell ran it. The release
# build aborts on a panic.
ABORTED = (-6, 134)


def known(name):
    return KNOWN.get(name)


def kill_group(process):
    """Kill `process` and everything it started, as a test binary cargo runs, and reap it."""
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def positive(value):
    """An argument above 0."""
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError(f"{value} is not above 0")
    return number


def variants(value):
    """`all`, or variants of the harness joined by commas."""
    unknown = [] if value == "all" else [name for name in value.split(",") if name not in VARIANTS]
    if unknown:
        named = ", ".join(f"'{name}'" for name in unknown)
        raise argparse.ArgumentTypeError(f"no variant named {named}: all, or {', '.join(VARIANTS)}")
    return value


class Campaign:
    def __init__(self, args, mode):
        self.args = args
        self.mode = mode
        self.root = Path(args.results) / f"{mode}-{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
        self.root.mkdir(parents=True, exist_ok=True)
        self.processes = 0
        self.violations = Counter()
        self.violation_logs = defaultdict(list)
        self.failures = Counter()
        self.notes = Counter()
        self.coverage = Counter()
        self.gaps = set()
        self.reports = Counter()
        self.started = time.time()
        self.rng = random.Random(args.seed)
        print(f"campaign {self.root} seed {args.seed}", flush=True)

    def out_of_time(self):
        return time.time() - self.started >= self.args.minutes * 60

    def log_path(self, label):
        self.processes += 1
        return self.root / f"{self.processes:05d}-{label}.log"

    def start(self, command, log):
        """Start `command` in a process group of its own, its command line and output to `log`, and
        return the process."""
        with open(log, "ab") as output:
            output.write((" ".join(command) + "\n").encode())
            output.flush()
            return subprocess.Popen(command, cwd=REPO, stdout=output, stderr=subprocess.STDOUT,
                                    start_new_session=True)

    def violation(self, name, log, count=1):
        new = name not in self.violations
        self.violations[name] += count
        self.violation_logs[name].append(log.name)
        if new:
            marker = f"known: {known(name)}" if known(name) else "NEW"
            print(f"  {log.name}: {marker} {name}", flush=True)

    def read_chaos(self, log, code):
        """Count a chaos process's lines: its violations by class, a panic by where it panicked and
        a hang or an abort by the mix of the run it ended, its coverage, its gaps, and its exit as a
        harness failure where it is not one its run allows; `code` is None for a process the driver
        killed. Returns the run the log ends in, where it ends in one."""
        lines = log.read_text(errors="replace").splitlines()
        running = None
        panicked = False
        for line in lines:
            if line.startswith("NOTE running "):
                running = line[len("NOTE running "):]
            elif line.startswith("RUN "):
                running = None
            elif line.startswith("VIOLATION "):
                self.violation(line[len("VIOLATION "):].split(":")[0], log)
            elif "panicked at" in line:
                panicked = True
                where = line.split("panicked at")[-1].strip().rstrip(":")[:80]
                self.violation(f"panic: {where}", log)
                self.notes[f"panicked in {running or 'no run'}"] += 1
            elif line.startswith("HANG"):
                mix = running.split()[0] if running else "unknown"
                self.violation(f"{mix}-hang", log)
                self.notes[f"hung in {running or 'no run'}"] += 1
            elif line.startswith("COVERAGE-GAP "):
                self.gaps.add(line[len("COVERAGE-GAP "):])
            elif line.startswith("COVERAGE "):
                *key, count = line[len("COVERAGE "):].split()
                if key[0] in ("mix", "variant"):
                    # `mix <name> <runs> <passed>`, and the same for a variant
                    self.coverage[f"{key[0]} {key[1]} runs"] += int(key[2])
                    self.coverage[f"{key[0]} {key[1]} passed"] += int(count)
                else:
                    self.coverage[" ".join(key)] += int(count)
            elif line.startswith("REPORT runs "):
                fields = line.split()
                for name, value in zip(fields[1::2], fields[2::2]):
                    self.reports[name] += int(value)
        # 0 clean, 1 violations, 99 stopped at one, 100 a hang: each already counted from its lines.
        if code in ABORTED:
            if not panicked:
                mix = running.split()[0] if running else "unknown"
                self.violation(f"{mix}-abort", log)
                self.notes[f"aborted in {running or 'no run'}"] += 1
        elif code is not None and code not in (0, 1, 99, 100):
            self.failures[f"exit-{code}"] += 1
            print(f"  {log.name}: exit {code}", flush=True)
        return running

    def read_suite(self, suite, log, code):
        """Count a deterministic suite's failed tests, each a violation of its own, and an exit that
        failed with no test failed, as a crash or a hang leaves, as a harness failure."""
        text = log.read_text(errors="replace")
        failed = re.findall(r"^test (\S+) \.\.\. FAILED$", text, re.MULTILINE)
        for name in failed:
            self.violation(f"{suite} {name}", log)
        results = re.findall(r"^test result: \w+\. (\d+) passed; (\d+) failed", text, re.MULTILINE)
        for passed, failures in results:
            self.reports[f"{suite} passed"] += int(passed)
            self.reports[f"{suite} failed"] += int(failures)
        if code != 0 and not failed:
            self.failures[f"{suite}-exit-{code}"] += 1
            print(f"  {log.name}: exit {code} with no test failed", flush=True)

    def summary(self):
        minutes = (time.time() - self.started) / 60
        lines = [f"campaign {self.root.name}: {self.processes} processes in {minutes:.1f} min, seed {self.args.seed}"]
        lines.append("violations (logs listed for the first few):")
        for name, count in sorted(self.violations.items(), key=lambda item: -item[1]):
            label = f"known: {known(name)}" if known(name) else "NEW"
            logs = self.violation_logs[name]
            lines.append(f"  {name}: {count} in {len(logs)} logs [{label}] e.g. {', '.join(logs[:4])}")
        if not self.violations:
            lines.append("  none")
        lines.append("harness failures: " + (", ".join(f"{name} {count}" for name, count in
                                                      sorted(self.failures.items())) or "none"))
        if self.notes:
            lines.append("notes: " + ", ".join(f"{name} {count}" for name, count in sorted(self.notes.items())))
        if self.reports:
            lines.append("reports: " + ", ".join(f"{name} {count}" for name, count in sorted(self.reports.items())))
        if self.coverage:
            lines.append("coverage:")
            lines.extend(f"  {name}: {count}" for name, count in sorted(self.coverage.items()))
        gaps = campaign_gaps(self.gaps, self.coverage)
        lines.append("coverage gaps: " + (", ".join(gaps) if gaps else "none"))
        text = "\n".join(lines)
        (self.root / "summary.txt").write_text(text + "\n")
        (self.root / "summary.json").write_text(json.dumps({
            "processes": self.processes,
            "minutes": minutes,
            "violations": dict(self.violations),
            "violation_logs": {name: logs[:4] for name, logs in self.violation_logs.items()},
            "failures": dict(self.failures),
            "notes": dict(self.notes),
            "reports": dict(self.reports),
            "coverage": dict(self.coverage),
            "gaps": sorted(self.gaps),
        }, indent=1))
        print(text, flush=True)


def campaign_gaps(gaps, coverage):
    """The gaps no process covered: a `record <mix> <kind>` one process missed, another may have
    made."""
    return sorted(gap for gap in gaps if coverage.get(gap, 0) == 0)


def chaos(campaign):
    """Processes of `state_stress chaos`, `--jobs` at a time, each for `--chunk` seconds on a seed
    of its own drawn from the campaign's, over every mix or the ones given, until the time is up. A
    process still running well past its time, which the harness's own watchdog should have ended,
    is killed and counted as a hang of the run it was in."""
    args = campaign.args
    listed = subprocess.run([str(args.binary), "list"], capture_output=True, text=True)
    mixes = listed.stdout.split()
    unknown = [] if args.mix == "all" else [name for name in args.mix.split(",") if name not in mixes]
    if listed.returncode != 0 or unknown:
        campaign.failures[f"no-mix-{','.join(unknown) or 'list'}"] += 1
        print(f"  the harness lists no mix named {', '.join(unknown)}: {', '.join(mixes)}", flush=True)
        return
    running = []
    while not campaign.out_of_time() or running:
        while len(running) < args.jobs and not campaign.out_of_time():
            seed = campaign.rng.randrange(1, 2 ** 63)
            left = max(10, int(args.minutes * 60 - (time.time() - campaign.started)))
            seconds = min(args.chunk, left)
            log = campaign.log_path(f"chaos-{seed}")
            command = [str(args.binary), "chaos", "--seconds", str(seconds), "--seed", str(seed),
                       "--mix", args.mix, "--variant", args.variant, "--hang-seconds", str(args.hang_seconds)]
            # A process stopped at a known class would cut its chunk short each time the class recurs.
            if args.stop_on_new and not KNOWN:
                command.append("--stop-on-violation")
            deadline = time.time() + seconds + 2 * args.hang_seconds
            running.append((campaign.start(command, log), log, deadline))
        time.sleep(0.5)
        for entry in list(running):
            process, log, deadline = entry
            code = process.poll()
            if code is None and time.time() < deadline:
                continue
            running.remove(entry)
            before = set(campaign.violations)
            if code is None:
                kill_group(process)
                run = campaign.read_chaos(log, None)
                campaign.violation(f"{run.split()[0] if run else 'unknown'}-hang", log)
                campaign.notes[f"killed past its time in {run or 'no run'}"] += 1
            else:
                campaign.read_chaos(log, code)
            if args.stop_on_new and any(known(name) is None for name in set(campaign.violations) - before):
                for other, other_log, _ in running:
                    kill_group(other)
                    campaign.read_chaos(other_log, None)
                return


def suites_once(campaign):
    """Each deterministic suite once."""
    for suite, command in SUITES.items():
        log = campaign.log_path(suite)
        process = campaign.start(command, log)
        try:
            code = process.wait(timeout=campaign.args.suite_timeout)
        except subprocess.TimeoutExpired:
            kill_group(process)
            campaign.violation(f"{suite} hang", log)
            continue
        campaign.read_suite(suite, log, code)


def scenarios(campaign):
    """The deterministic suites, once each."""
    suites_once(campaign)


def soak(campaign):
    """The deterministic suites, each in turn, again and again until the time is up."""
    rounds = 0
    while not campaign.out_of_time():
        suites_once(campaign)
        rounds += 1
    campaign.notes["rounds"] = rounds


def build(args):
    """The harness in the release build, which aborts on a panic, and the suites' tests."""
    subprocess.run(["cargo", "build", "--release", "--example", "state_stress", "-p", "lore-revision",
                    "--features", "state-stress"], cwd=REPO, check=True)
    for command in SUITES.values():
        subprocess.run([*command, "--no-run"], cwd=REPO, check=True)


def report(results):
    """Merge the summaries of every campaign under `results`: violations and failures with the
    campaigns that found them, coverage summed, and the gaps no campaign covered."""
    violations, failures, notes, reports, coverage = Counter(), Counter(), Counter(), Counter(), Counter()
    found_in = defaultdict(list)
    gaps = set()
    processes = 0
    campaigns = sorted(Path(results).glob("*/summary.json"))
    for path in campaigns:
        summary = json.loads(path.read_text())
        name = path.parent.name
        processes += summary["processes"]
        for kind, count in summary["violations"].items():
            violations[kind] += count
            found_in[kind].append(name)
        failures.update(summary["failures"])
        notes.update(summary.get("notes", {}))
        reports.update(summary.get("reports", {}))
        coverage.update(summary.get("coverage", {}))
        gaps.update(summary.get("gaps", []))
    lines = [f"{len(campaigns)} campaigns, {processes} processes"]
    lines.append("violations:")
    for kind, count in sorted(violations.items(), key=lambda item: -item[1]):
        label = f"known: {known(kind)}" if known(kind) else "NEW"
        lines.append(f"  {kind}: {count} [{label}] in {', '.join(found_in[kind][:4])}")
    if not violations:
        lines.append("  none")
    lines.append("harness failures: " + (", ".join(f"{k} {v}" for k, v in sorted(failures.items())) or "none"))
    lines.append("notes: " + (", ".join(f"{k} {v}" for k, v in sorted(notes.items())) or "none"))
    lines.append("reports: " + (", ".join(f"{k} {v}" for k, v in sorted(reports.items())) or "none"))
    lines.append("coverage:")
    lines.extend(f"  {kind}: {count}" for kind, count in sorted(coverage.items()))
    left = campaign_gaps(gaps, coverage)
    lines.append("coverage gaps: " + (", ".join(left) if left else "none"))
    print("\n".join(lines))
    return 1 if violations or failures else 0


MODES = {
    "chaos": chaos,
    "scenarios": scenarios,
    "soak": soak,
}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=[*MODES, "all", "build", "report"])
    parser.add_argument("dir", nargs="?", help="for report: the results directory to merge")
    parser.add_argument("--minutes", type=float, default=10, help="time budget per mode")
    parser.add_argument("--results", default=str(REPO / "target" / "state-stress" / "results"))
    parser.add_argument("--binary", default=str(REPO / "target" / "release" / "examples" / "state_stress"))
    parser.add_argument("--seed", type=int, default=int(time.time()))
    parser.add_argument("--jobs", type=positive, default=max(1, (os.cpu_count() or 8) // 8),
                        help="chaos processes at once; each runs up to twice the cores' workers")
    parser.add_argument("--chunk", type=positive, default=300, help="seconds per chaos process")
    parser.add_argument("--mix", default="all", help="the mixes chaos runs: all, or names joined by commas")
    parser.add_argument("--variant", type=variants, default="all", help="the variants chaos runs draw from: all, "
                        "or plain, drop and fault joined by commas")
    parser.add_argument("--hang-seconds", type=positive, default=300, help="a chaos process with no run ending for this "
                        "long exits as hung")
    parser.add_argument("--suite-timeout", type=positive, default=3600, help="seconds a deterministic suite may run")
    parser.add_argument("--stop-on-new", action="store_true", help="chaos: stop at the first violation class not in "
                        "KNOWN")
    args = parser.parse_args()
    if args.mode == "build":
        build(args)
        return 0
    if args.mode == "report":
        return report(args.dir or args.results)
    modes = list(MODES) if args.mode == "all" else [args.mode]
    found = False
    for mode in modes:
        campaign = Campaign(args, mode)
        MODES[mode](campaign)
        campaign.summary()
        found = found or bool(campaign.violations or campaign.failures)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
