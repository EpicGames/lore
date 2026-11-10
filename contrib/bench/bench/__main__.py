"""The bench command.

    bench validate SPEC                 check a spec and print its identity
    bench prepare SPEC [--run-id ID]    install the harness, then fingerprint, apply settings, fetch
                                        tools and the corpus (in the foreground)
    bench run SPEC [--run-id ID] [--foreground]
                                        prepare, calibrate and run the matrix; detached on the client
                                        unless --foreground. An existing --run-id resumes, from the
                                        same spec only.
    bench status SPEC [--run-id ID]     the run's log so far, and whether its agent is running
    bench watch SPEC [--run-id ID]      follow a run: new log lines, progress every few minutes, a
                                        warning when nothing moves, and exit when the agent stops
    bench collect SPEC [--run-id ID] [--dest DIR]
                                        copy the run's results here
    bench report RUN_DIR... [--trend] [--out FILE]

State (the harness's SSH key, known hosts, the latest run per spec, collected results) lives in
$BENCH_STATE, by default ~/.local/share/bench. Run from contrib/bench: python3 -m bench ...
"""
import argparse
import io
import json
import os
import shlex
import subprocess
import sys
import tarfile
import tempfile
import time

from . import remote, report, util
from . import spec as specmod

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATE = os.environ.get("BENCH_STATE", os.path.expanduser("~/.local/share/bench"))
KEY = os.path.join(STATE, "bench_ed25519")


def hosts(spec):
    inv = spec["_inventory"]
    kh = os.path.join(STATE, "known_hosts")
    return {role: remote.from_inventory(inv[role], remote.harness_dir(inv[role]), KEY, kh) for role in ("client", "server")}


def harness_tarball():
    """The harness (bench/ and host/) as a gzip tarball, and the hash of its content."""
    files = []
    for top in ("bench", "host"):
        for dirpath, dirnames, filenames in os.walk(os.path.join(HERE, top)):
            dirnames[:] = sorted(d for d in dirnames if d != "__pycache__")
            files += [os.path.join(dirpath, f) for f in sorted(filenames) if not f.endswith(".pyc")]
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for f in files:
            tar.add(f, arcname=os.path.relpath(f, HERE))
    sha = util.digest([[os.path.relpath(f, HERE), util.sha256_file(f)] for f in files])
    return buf.getvalue(), sha


def install(spec, hs):
    data, sha = harness_tarball()
    with tempfile.NamedTemporaryFile(suffix=".tar.gz") as tmp:
        tmp.write(data)
        tmp.flush()
        for role, h in hs.items():
            h.run(["mkdir", "-p", h.harness])
            h.put(tmp.name, f"{h.harness}.tar.gz")
            h.run(f"tar -xzf {shlex.quote(h.harness)}.tar.gz -C {shlex.quote(h.harness)} && rm {shlex.quote(h.harness)}.tar.gz")
    hs["client"].run("mkdir -p ~/.ssh && chmod 700 ~/.ssh")
    hs["client"].put(KEY, ".ssh/bench_ed25519")
    hs["client"].run("chmod 600 ~/.ssh/bench_ed25519")
    return sha


def run_dir_for(spec, run_id):
    return os.path.join(spec["_inventory"]["client"]["data"], "runs", run_id)


def latest_path(spec):
    return os.path.join(STATE, "runs", f"{spec['name']}.latest")


def resolve_run_id(spec, given, new):
    if given:
        return given
    path = latest_path(spec)
    if not new and os.path.exists(path):
        return open(path).read().strip()
    return f"{spec['name']}-{time.strftime('%Y%m%d-%H%M%S', time.gmtime())}"


def run_state(client, rd):
    """The run's state on the client: log lines, heartbeat, whether its agent is alive, how it ended."""
    return json.loads(client.run(["python3", "-c", PROBE, rd], check=False, timeout=60).stdout)


def check_resume(client, rd, spec):
    """Refuse to start an agent on the run in RD (on CLIENT) when it exists and SPEC isn't the spec
    it started from, or when its agent is running. Returns the spec it started from, or None for a
    new run."""
    res = client.run(["cat", os.path.join(rd, "spec.json")], check=False)
    if res.returncode != 0:
        return None
    before = json.loads(res.stdout)
    started = before.get("_sha256") or ""
    run_id = os.path.basename(rd)
    if started != spec["_sha256"]:
        raise remote.HostError(f"run {run_id} started from spec {started[:12]}, not this one ({spec['_sha256'][:12]}). "
                               "Resume it with the spec it started from, or start a new run.")
    if run_state(client, rd)["alive"]:
        raise remote.HostError(f"run {run_id} has an agent running; follow it with bench watch")
    return before


def stage(spec, run_id):
    """Install the harness on both hosts and put the resolved spec in the run directory."""
    hs = hosts(spec)
    rd = run_dir_for(spec, run_id)
    started = check_resume(hs["client"], rd, spec)
    spec["_harness_sha256"] = install(spec, hs)
    if started and started.get("_harness_sha256") != spec["_harness_sha256"]:
        print(f"note: run {run_id} started with harness {(started.get('_harness_sha256') or '')[:12]} and resumes with "
              f"{spec['_harness_sha256'][:12]}; each result records its own, and the report lists both", file=sys.stderr)
    hs["client"].run(["mkdir", "-p", rd])
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as tmp:
        tmp.close()
        util.write_json(tmp.name, spec)
        hs["client"].put(tmp.name, os.path.join(rd, "spec.json"))
        os.remove(tmp.name)
    os.makedirs(os.path.dirname(latest_path(spec)), exist_ok=True)
    with open(latest_path(spec), "w") as f:
        f.write(run_id + "\n")
    return hs, rd


def cmd_validate(args):
    spec = specmod.load(args.spec)
    inv = spec.get("_inventory", {})
    print(f"{spec['name']}: spec {spec['_sha256']}")
    print(f"  {len(spec['profile'])} profiles, tools {', '.join(spec['tool'])}, tests {', '.join(spec['run']['tests'])}, "
          f"{spec['run']['repetitions']} repetitions: {sum(1 for _ in specmod.matrix(spec))} runs")
    for role, h in inv.items():
        print(f"  {role}: {h['user']}@{h['host']}:{h['port']}{' via ' + h['jump'] if h.get('jump') else ''}, "
              f"ip {h['ip']}, data {h['data']}")
    return 0


def cmd_prepare(args):
    spec = specmod.load(args.spec)
    run_id = resolve_run_id(spec, args.run_id, new=args.run_id is None)
    hs, rd = stage(spec, run_id)
    print(f"run {run_id}")
    c = hs["client"]
    res = c.run(f"cd {shlex.quote(c.harness)} && python3 -m bench agent prepare {shlex.quote(rd)}", check=False,
                capture=False, timeout=None)
    return res.returncode


def cmd_run(args):
    spec = specmod.load(args.spec)
    run_id = resolve_run_id(spec, args.run_id, new=args.run_id is None)
    hs, rd = stage(spec, run_id)
    c = hs["client"]
    agent = f"cd {shlex.quote(c.harness)} && python3 -m bench agent run {shlex.quote(rd)}"
    if args.foreground:
        return c.run(agent, check=False, capture=False, timeout=None).returncode
    pid = c.run(f"setsid nohup bash -c {shlex.quote(agent)} > {shlex.quote(rd)}/agent.log 2>&1 < /dev/null & "
                f"echo $! > {shlex.quote(rd)}/agent.pid; cat {shlex.quote(rd)}/agent.pid").stdout.strip()
    print(f"run {run_id}: agent started on the client (pid {pid}). Follow it with: python3 -m bench status {args.spec}")
    return 0


def cmd_status(args):
    spec = specmod.load(args.spec)
    run_id = resolve_run_id(spec, args.run_id, new=False)
    rd = run_dir_for(spec, run_id)
    c = hosts(spec)["client"]
    out = c.run(f"tail -n {args.lines} {shlex.quote(rd)}/run.log 2>/dev/null", check=False).stdout
    state = run_state(c, rd)
    if state["alive"]:
        out += "agent running\n"
    else:
        out += outcome(state["end"])[0] + "\n"
        out += c.run(f"tail -n 5 {shlex.quote(rd)}/agent.log 2>/dev/null", check=False).stdout
    print(f"run {run_id}\n{out}", end="")
    return 0


# Run on the client by `bench watch`: the run's state as one line of JSON.
PROBE = """
import fcntl, json, os, sys, time
rd = sys.argv[1]
def read(name, default):
    try:
        return open(os.path.join(rd, name)).read()
    except OSError:
        return default
lines = read("run.log", "").count("\\n")
beat = json.loads(read("heartbeat.json", "{}") or "{}")
end = json.loads(read("end.json", "null") or "null")
# An agent holds agent.lock while it runs, detached or in the foreground.
try:
    with open(os.path.join(rd, "agent.lock"), "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    alive = False
except BlockingIOError:
    alive = True
except OSError:
    alive = False
print(json.dumps({"lines": lines, "beat": beat, "alive": alive, "end": end, "now": time.time()}))
"""


def outcome(end):
    """What `bench watch` says when the agent has stopped, and its exit code, from the agent's
    end.json (END): finished, stopped with an exit code or an error, or gone without a word."""
    if end is None:
        return "AGENT STOPPED without recording how: it was killed, or its host restarted", 1
    if end["exit"] == 0:
        return "RUN FINISHED", 0
    return f"AGENT STOPPED: exit {end['exit']}{': ' + end['error'] if end.get('error') else ''}", 1


def stall_notes(alive, step, beat_age, line_age, stall):
    """Why a run looks stuck, for `bench watch`: a step with no progress for STALL seconds, a step's
    heartbeat older than STALL or 5 minutes, or no log line for STALL or 10 minutes outside a step.
    Heartbeats come during steps only, so outside one their age says nothing."""
    notes = []
    if step and step["since_progress_s"] > stall:
        notes.append(f"step {step['step']} has made no progress for {step['since_progress_s']} s")
    if alive and step and beat_age > max(stall, 300):
        notes.append(f"no heartbeat for {round(beat_age)} s")
    if alive and not step and line_age > max(stall, 600):
        notes.append(f"no log line for {round(line_age)} s outside a step")
    return notes


def cmd_watch(args):
    """Follow a run and say so whenever it isn't visibly moving. Prints new log lines and a
    progress line every --every seconds. Warns when a step has made no progress for --stall
    seconds, when a step's heartbeat is older than 5 minutes, or when no log line has come for 10
    minutes outside a step (longer if --stall is). Exits when the agent stops: 0 when every run is
    done, 1 otherwise."""
    spec = specmod.load(args.spec)
    run_id = resolve_run_id(spec, args.run_id, new=False)
    rd = shlex.quote(run_dir_for(spec, run_id))
    c = hosts(spec)["client"]
    seen = 0
    last_line_at = last_report = time.time()
    warned, failures = 0.0, 0
    print(f"watching run {run_id}", flush=True)
    while True:
        try:
            state = run_state(c, run_dir_for(spec, run_id))
            lines, beat, alive, remote_now = state["lines"], state["beat"], state["alive"], state["now"]
            new = c.run(f"tail -n +{seen + 1} {rd}/run.log | head -n {lines - seen}", check=False,
                        timeout=60).stdout if lines > seen else ""
        except Exception as e:  # an unreachable client is itself worth saying, and not for ever
            failures += 1
            print(f"WATCH: cannot read the run's state ({failures}): {type(e).__name__}: {e}", flush=True)
            if failures >= 10:
                print("WATCH: giving up after 10 failed checks in a row", flush=True)
                return 2
            time.sleep(args.interval)
            continue
        failures = 0
        now = time.time()
        if lines > seen:
            for line in new.splitlines():
                print(line, flush=True)
            seen, last_line_at = lines, now
        step = beat.get("step")
        still = stall_notes(alive, step, remote_now - beat.get("time", remote_now), now - last_line_at, args.stall)
        if still and now - warned > args.stall:
            print("STALL? " + "; ".join(still), flush=True)
            warned = now
        if not alive:
            message, code = outcome(state["end"])
            try:
                tail = c.run(f"tail -n 3 {rd}/run.log; tail -n 5 {rd}/agent.log", check=False, timeout=60).stdout
            except remote.HostError as e:
                tail = f"(the logs' last lines: {e})"
            print(message + "\n" + tail, flush=True)
            return code
        if now - last_report > args.every:
            if step:
                print(f"progress: {step['step']} running {util.mmss(step['elapsed_s'])}, {step['net_mbit']} Mbit/s, "
                      f"{step['cpu_cores']} cores, last progress {step['since_progress_s']} s ago", flush=True)
            else:
                print(f"progress: between steps, last log line {round(now - last_line_at)} s ago", flush=True)
            last_report = now
        time.sleep(args.interval)


def cmd_collect(args):
    spec = specmod.load(args.spec)
    run_id = resolve_run_id(spec, args.run_id, new=False)
    rd = run_dir_for(spec, run_id)
    dest = args.dest or os.path.join(STATE, "results")
    os.makedirs(dest, exist_ok=True)
    hosts(spec)["client"].get_dir(rd, os.path.join(dest, run_id), compress=True)
    print(os.path.join(dest, run_id))
    return 0


def cmd_report(args):
    if len(args.run_dirs) > 1 and not args.trend:
        print("report: several run directories make a trend; add --trend", file=sys.stderr)
        return 2
    text = report.trend(args.run_dirs) if args.trend else report.matrix(args.run_dirs[0])
    out = args.out or os.path.join(args.run_dirs[0], "trend.html" if args.trend else "report.html")
    with open(out, "w") as f:
        f.write(text)
    print(out)
    return 0


def main(argv):
    if argv and argv[0] == "hostops":
        from . import hostops
        return hostops.main(argv[1:])
    if argv and argv[0] == "agent":
        from . import agent
        return agent.main(argv[1:])
    p = argparse.ArgumentParser(prog="bench", description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    for name, fn in (("validate", cmd_validate), ("prepare", cmd_prepare), ("run", cmd_run),
                     ("status", cmd_status), ("watch", cmd_watch), ("collect", cmd_collect)):
        sp = sub.add_parser(name)
        sp.add_argument("spec")
        sp.set_defaults(fn=fn)
        if name != "validate":
            sp.add_argument("--run-id")
        if name == "run":
            sp.add_argument("--foreground", action="store_true")
        if name == "status":
            sp.add_argument("--lines", type=int, default=25)
        if name == "collect":
            sp.add_argument("--dest")
        if name == "watch":
            sp.add_argument("--interval", type=int, default=30, help="seconds between checks")
            sp.add_argument("--stall", type=int, default=180, help="seconds without progress before a warning")
            sp.add_argument("--every", type=int, default=300, help="seconds between progress lines")
    sp = sub.add_parser("report")
    sp.add_argument("run_dirs", nargs="+")
    sp.add_argument("--trend", action="store_true")
    sp.add_argument("--out")
    sp.set_defaults(fn=cmd_report)
    args = p.parse_args(argv)
    try:
        return args.fn(args)
    except specmod.SpecError as e:
        print("spec errors:\n" + "\n".join(f"  {m}" for m in e.errors), file=sys.stderr)
        return 1
    except remote.HostError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
