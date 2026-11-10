"""The agent: runs a resolved spec on the client host, reaching the server over SSH.

    python3 -m bench agent prepare RUN_DIR    fingerprint, settings, tools and corpus
    python3 -m bench agent run RUN_DIR        prepare, then calibrate and run every profile

RUN_DIR holds spec.json, which the operator's `bench run` wrote. Results go in RUN_DIR: a directory
per profile with calibration.json and one directory per run with result.json and the raw files
behind it. A run with a result and no failed step or verification is skipped, so starting the agent
again resumes a matrix and repeats what failed. One agent at a time works on a run directory, and
it records how it ended in end.json.
"""
import fcntl
import os
import shutil
import signal
import subprocess
import sys
import time

from . import METHOD, adapters, calibrate, corpus, fetch, util
from . import spec as specmod
from .remote import Host

TOOL_BINARIES = {"p4": {"client": ["p4"], "server": ["p4d", "p4"]},
                 "lore": {"client": ["lore"], "server": ["loreserver"]}}
# Shaping a profile corrects the delay by the shaped round trip at most this many times, and stops
# once a correction would change the round trip by no more than SHAPE_CLOSE_ENOUGH_US, about the
# spread of the median of 20 connections.
SHAPE_CORRECTIONS = 2
SHAPE_CLOSE_ENOUGH_US = 100


class Agent:
    def __init__(self, run_dir):
        self.run_dir = os.path.abspath(run_dir)
        self.spec = util.read_json(os.path.join(self.run_dir, "spec.json"))
        inv = self.spec["_inventory"]
        self.inv = inv
        self.log = util.Log(os.path.join(self.run_dir, "run.log"))
        self.client = Host("client", "localhost", os.environ.get("USER", "bench"), local=True,
                           harness=os.path.join(inv["client"]["data"], "harness"))
        self.client.data = inv["client"]["data"]
        s = inv["server"]
        self.server = Host("server", s["ip"], s["user"], s["port"], key=os.path.expanduser("~/.ssh/bench_ed25519"),
                           known_hosts=os.path.expanduser("~/.ssh/bench_known_hosts"),
                           harness=os.path.join(s["data"], "harness"))
        self.server.data = s["data"]
        self.run_id = os.path.basename(self.run_dir)
        self.server_run_dir = os.path.join(s["data"], "runs", self.run_id)
        self.poll_s = 10
        self.kill_grace_s = 30
        self.dev = self.disk = None
        self.watch_problem = None
        self.shaped = {}  # per profile: the unshaped round trip measured for it, and the delay added
        try:
            from . import hostops
            self.dev, self.disk = hostops.conf()["DEV"], hostops.disk_name(self.client.data)
        except (OSError, KeyError, RuntimeError, ValueError) as e:
            # Not on a prepared host, as in tests. A run says so: its watchdog sees CPU time only.
            self.watch_problem = f"{type(e).__name__}: {e}"

    def lock(self):
        """Hold RUN_DIR/agent.lock for the rest of this process, so a second agent can't start on
        the same run."""
        self._lock = open(os.path.join(self.run_dir, "agent.lock"), "w")
        try:
            fcntl.flock(self._lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError(f"another agent is working on {self.run_dir}") from None

    # ---- preparation

    def prepare(self):
        """Fingerprint both hosts, apply the spec's settings, and fetch the tools and the corpus.
        Resuming, the settings are applied again (a host may have restarted), the hosts must be the
        kind they were, and the tools and the corpus are fetched again where they are gone, as from
        an instance store after a stop: the corpus must then be the one the run started with."""
        pdir = os.path.join(self.run_dir, "prepare")
        done = os.path.join(pdir, "prepared.json")
        before = util.read_json(done) if os.path.exists(done) else None
        os.makedirs(pdir, exist_ok=True)
        self.log(f"{'resuming' if before else 'preparing'} run {self.run_id}")
        self.server.op("stop-servers", os.path.join(self.server.data, "srv"))
        self.server.op("unshape")
        self.client.op("unshape")
        out = {"sysctl": {}, "fingerprint": {}}
        for host in (self.client, self.server):
            out["sysctl"][host.role] = host.op("sysctl", input=self.spec["host"]["sysctl"])
            out["fingerprint"][host.role] = host.op("fingerprint", host.data)
        for role, fp in out["fingerprint"].items():
            self.log(f"{role}: {fp['cpus']} CPUs ({fp['cpu_model']}), {fp['mem_bytes'] / 2**30:.0f} GiB, "
                     f"kernel {fp['kernel']}, {fp['virt']}, {fp['disk']['fstype']} on {fp['disk']['device']}")
        stale = stale_helpers(out["fingerprint"])
        if stale:
            raise RuntimeError(f"the bench-helper installed on the {' and the '.join(stale)} isn't this harness's "
                               "host/bench-helper, so it would shape the link another way than the results record. "
                               "Run host/bootstrap.sh on the host again, or start its VM again with vm/vm.sh up.")
        if before:
            changed = changed_class(before["fingerprint"], out["fingerprint"])
            if changed:
                raise RuntimeError("the hosts changed since the run started, so its results wouldn't compare: "
                                   + "; ".join(changed) + ". Start a new run.")
            tools = self.resolve_tools()
            info = corpus.prepare(self.spec["corpus"], self.client.data, self.log)
            if info["id"] != before["corpus"]["id"]:
                raise RuntimeError(f"the corpus is {info['id'][:12]}, not the {before['corpus']['id'][:12]} the run "
                                   "started with. Start a new run.")
            self.prepared = dict(before, sysctl=out["sysctl"], tools=tools, corpus=info)
            return self.prepared
        out["base_rtt_ms"] = self.base_rtt()
        self.log(f"unshaped round trip {out['base_rtt_ms']['median']:.2f} ms")
        out["tools"] = self.resolve_tools()
        out["corpus"] = corpus.prepare(self.spec["corpus"], self.client.data, self.log)
        util.write_json(done, out)
        self.prepared = out
        return out

    def base_rtt(self):
        """The round trip as the link is shaped now, unshaped before a profile: the time a TCP
        connection to the server's iperf3 takes to open."""
        self.server.op("iperf-start", calibrate.IPERF_PORT)
        try:
            return calibrate.stats(calibrate.connect_rtts(self.inv["server"]["ip"], calibrate.IPERF_PORT, 20))
        finally:
            self.server.op("iperf-stop", calibrate.IPERF_PORT)

    def resolve_tools(self):
        """Fetch every binary the spec pins into the client's cache, and copy the server's to the
        server, where they land in the same place."""
        cache = os.path.join(self.client.data, "cache", "tools")
        server_cache = os.path.join(self.server.data, "cache", "tools")
        out = {}
        for tool, settings in self.spec["tool"].items():
            need = TOOL_BINARIES[tool]
            entry = {"client": {}, "server": {}, "sha256": {}}
            for name in sorted(set(need["client"] + need["server"])):
                b = settings[name]
                path = executable(fetch.fetch(b["url"], b["sha256"], cache, name, self.log), cache, b["sha256"], name)
                entry["sha256"][name] = b["sha256"]
                if name in need["client"]:
                    entry["client"][name] = path
                if name in need["server"]:
                    remote = os.path.join(server_cache, b["sha256"], name)
                    if not self.server.op("have", remote, b["sha256"])["ok"]:
                        self.log(f"copying {name} to the server")
                        self.server.run(["mkdir", "-p", os.path.dirname(remote)])
                        self.server.put(path, remote)
                        self.server.run(["chmod", "755", remote])
                        if not self.server.op("have", remote, b["sha256"])["ok"]:
                            raise RuntimeError(f"{name} arrived on the server with the wrong content")
                    entry["server"][name] = remote
            out[tool] = entry
        return out

    # ---- the matrix

    def run(self):
        """Prepare, then every profile: shape, calibrate, and run what isn't done. Returns 0 when every
        run is done, 1 when a calibration stopped a profile."""
        self.prepare()
        if self.watch_problem:
            self.log(f"watchdog: can't read the network card or data disk ({self.watch_problem}); "
                     "it sees the steps' CPU time only")
        stopped = []
        try:
            for profile in self.spec["profile"]:
                runs = [(rep, pos, tool) for p, rep, pos, tool in specmod.matrix(self.spec) if p["name"] == profile["name"]]
                if all(self.done(profile, tool, rep) for rep, _, tool in runs):
                    self.log(f"profile {profile['name']}: every run is done")
                    continue
                self.shape(profile)
                if not self.calibrate(profile):
                    stopped.append(profile["name"])
                    continue
                for rep, pos, tool in runs:
                    if self.done(profile, tool, rep):
                        self.log(f"{profile['name']}/{tool}-r{rep}: done already")
                        continue
                    self.run_one(profile, rep, pos, tool)
        finally:
            for host in (self.client, self.server):
                try:
                    host.op("unshape")
                except Exception as e:  # leave the hosts unshaped if at all possible, but report it
                    self.log(f"unshaping the {host.role} failed: {e}")
            try:
                self.server.op("stop-servers", os.path.join(self.server.data, "srv"))
            except Exception as e:
                self.log(f"stopping the servers failed: {e}")
        if stopped:
            self.log(f"finished; the calibration stopped profile {', '.join(stopped)}")
            return 1
        self.log("all runs done")
        return 0

    def result_path(self, profile, tool, rep):
        return os.path.join(self.run_dir, profile["name"], f"{tool}-r{rep}", "result.json")

    def done(self, profile, tool, rep):
        """Whether a run has a result that needs no repeat: a failed step or verification is run
        again when the agent resumes."""
        path = self.result_path(profile, tool, rep)
        if not os.path.exists(path):
            return False
        phases = util.read_json(path)["phases"].values()
        return not any({"step-failed", "verify-failed"} & set(p["flags"]) for p in phases)

    def shape(self, profile):
        """Shape the link to PROFILE, so that the round trip the tools see is the profile's. The
        shaper adds what the unshaped link lacks of it, measured again for each profile: a VM's
        network path adds tenths of a millisecond, and how much drifts over a run of hours. The
        shaped link is then slower than the two added up, since the shaper takes time of its own and
        the unshaped round trip varies between measurements, so the delay is corrected by what the
        shaped round trip misses, up to SHAPE_CORRECTIONS times."""
        for host in (self.client, self.server):
            host.op("unshape")
        base = self.base_rtt()
        rtt, rate = profile["rtt_ms"], profile.get("rate_mbit") or 0
        delay_us = shaping_delay_us(rtt, base.get("median"))
        jitter_us = int(round(profile.get("jitter_ms", 0) * 1000 / 2))
        queue = specmod.queue_bytes(profile)

        def apply(delay):
            for host, peer in ((self.client, self.inv["server"]["ip"]), (self.server, self.inv["client"]["ip"])):
                host.op("shape", peer, delay, rate, queue, profile.get("loss_ppm", 0), jitter_us)

        apply(delay_us)
        shaped = self.base_rtt()
        for _ in range(SHAPE_CORRECTIONS):
            corrected = max(0, delay_us + int(round((rtt - shaped.get("median", rtt)) * 1000 / 2)))
            if abs(corrected - delay_us) * 2 <= SHAPE_CLOSE_ENOUGH_US:
                break
            delay_us = corrected
            apply(delay_us)
            shaped = self.base_rtt()
        self.shaped[profile["name"]] = {"base_rtt_ms": base, "delay_us_each_way": delay_us, "shaped_rtt_ms": shaped}
        self.log(f"profile {profile['name']}: {rtt} ms round trip, {base.get('median', 0):.2f} ms of it unshaped; "
                 f"{delay_us} us added each way gives {shaped.get('median', 0):.2f} ms; "
                 f"{f'{rate} Mbit/s, queue {queue} bytes' if rate else 'no rate limit'}")

    def calibrate(self, profile):
        """Calibrate PROFILE's link and the disks, every time an agent starts on the profile: after
        an interruption the link may not be what it was. Returns whether the profile may run."""
        pdir = os.path.join(self.run_dir, profile["name"])
        os.makedirs(pdir, exist_ok=True)
        path = os.path.join(pdir, "calibration.json")
        tol = self.spec["run"]["calibration"]
        shaped = self.shaped.get(profile["name"], {})
        base = shaped.get("base_rtt_ms", self.prepared["base_rtt_ms"]).get("median")
        try:
            result, problems = calibrate.run(self.client, self.server, self.inv["server"]["ip"], profile, tol,
                                             self.log, base)
        except Exception as e:  # a profile's failed calibration is that profile's, not the run's
            result, problems = {"profile": profile["name"]}, [f"calibration failed: {type(e).__name__}: {e}"]
            tol = dict(tol, enforce=True)
        result.update(shaped)
        result["problems"] = problems
        result["ok"] = not problems or not tol["enforce"]
        util.write_json(path, result)
        for p in problems:
            self.log(f"calibration: {p}")
        if problems and tol["enforce"]:
            self.log(f"profile {profile['name']} stopped by the calibration gate")
            return False
        return True

    def cold(self):
        """Every phase starts cold: both data volumes TRIMmed, a settle time for the drives, both page
        caches dropped."""
        for host in (self.client, self.server):
            host.op("trim")
        self.beat(None)
        time.sleep(self.spec["run"]["settle_s"])
        self.beat(None)
        for host in (self.client, self.server):
            host.op("dropcaches")

    def run_one(self, profile, rep, position, tool):
        name = f"{profile['name']}/{tool}-r{rep}"
        rd = os.path.join(self.run_dir, profile["name"], f"{tool}-r{rep}")
        shutil.rmtree(rd, ignore_errors=True)
        os.makedirs(rd)
        srd = os.path.join(self.server_run_dir, profile["name"], f"{tool}-r{rep}")
        self.server.run(["rm", "-rf", srd])
        self.server.run(["mkdir", "-p", srd])
        info = self.prepared["corpus"]
        tools = self.prepared["tools"][tool]
        work = os.path.join(self.client.data, "work", f"{tool}-r{rep}")
        ctx = adapters.Context(self.server, self.inv["server"]["ip"], self.server.data, tools["client"],
                               tools["server"], info, rd, work, rep, self.spec["tool"][tool],
                               self.spec["_files"], srd)
        adapter = adapters.make(tool, ctx)
        started = util.now()
        self.log(f"{name}: restore corpus")
        restored = corpus.restore(info)
        shutil.rmtree(work, ignore_errors=True)
        self.log(f"{name}: reset server")
        reset = adapter.reset_server()
        phases = {}
        for test in self.spec["run"]["tests"]:
            phases[test] = self.phase(name, test, adapter, rd, srd, rep)
            if any(s["exit"] != 0 for s in phases[test]["steps"]):
                break
        self.log(f"{name}: collecting the server's files")
        server_log = adapter.server_log()
        if server_log:
            self.server.run(f"cp {server_log} {srd}/server.log 2>/dev/null || true")
        self.server.get_dir(srd, os.path.join(rd, "server"))
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(os.path.join(info["root"], ".lore"), ignore_errors=True)
        fps = self.prepared["fingerprint"]
        infra = {role: fps[role]["class"] for role in ("client", "server")}
        for test, ph in phases.items():
            ph["series"] = specmod.series_key(self.spec, profile, test, tool, info["id"], infra)
        result = {
            "schema": 1, "method": METHOD, "run_id": self.run_id, "spec_name": self.spec["name"],
            "spec_sha256": self.spec["_sha256"], "harness_sha256": self.spec.get("_harness_sha256"),
            "profile": profile, "repetition": rep, "position": position, "started": started, "finished": util.now(),
            "tool": {"name": tool, "version": adapter.version(), "binaries": tools["sha256"],
                     "settings": specmod.tool_settings(self.spec, tool)},
            "corpus": {"id": info["id"], "files": info["files"], "bytes": info["bytes"]},
            "infra": {"class": util.digest(infra),
                      "fingerprint": {role: util.digest(fps[role]) for role in ("client", "server")}},
            "server_reset": reset, "corpus_restore": restored, "phases": phases,
        }
        util.write_json(os.path.join(rd, "result.json"), result)
        summary = "; ".join(f"{t} {util.mmss(p['wall_s'])}{' ' + ','.join(p['flags']) if p['flags'] else ''}"
                            for t, p in phases.items())
        self.log(f"{name}: done, {summary}")

    def phase(self, name, test, adapter, rd, srd, rep):
        self.log(f"{name}: {test} setup")
        (adapter.setup_import if test == "import" else adapter.setup_fetch)()
        steps = adapter.import_steps() if test == "import" else adapter.fetch_steps()
        self.log(f"{name}: {test}: cold caches")
        self.cold()
        wire0 = {"client": self.client.op("wire"), "server": self.server.op("wire")}
        results, mon, started = [], {}, []
        try:
            for host, d, pidfile in ((self.client, rd, ""), (self.server, srd, adapter.server_pidfile)):
                host.op("monitor-start", d, test, pidfile)
                started.append((host, d))
            for step_name, argv, cwd, env in steps:
                self.log(f"{name}: {step_name}")
                results.append(self.step(rd, step_name, argv, cwd, env))
                self.log(f"{name}: {step_name} exit {results[-1]['exit']}, {util.mmss(results[-1]['wall_s'])}")
                if results[-1]["exit"] != 0:
                    break
            # Not part of the tool's time: how long the writes it left in memory take to reach the disks.
            flush = {"client": self.client.op("flush")["seconds"], "server": self.server.op("flush")["seconds"]}
        except BaseException:
            # Every monitor that started is stopped, or it would record on into later runs as the
            # benchmark user's load.
            for host, d in started:
                try:
                    host.op("monitor-stop", d, test, host.data)
                except Exception as e:
                    self.log(f"stopping the {host.role}'s monitors failed: {e}")
            raise
        for host, d in started:
            mon[host.role] = host.op("monitor-stop", d, test, host.data)
        wire1 = {"client": self.client.op("wire"), "server": self.server.op("wire")}
        delta = {role: {k: _delta(wire1[role].get(k), wire0[role].get(k)) for k in wire1[role]} for role in wire1}
        ph = {
            "steps": results,
            "wall_s": sum(s["wall_s"] for s in results),
            "client_cpu_s": sum(s["user_s"] + s["sys_s"] for s in results),
            "client_peak_rss_bytes": max((s["max_rss_bytes"] for s in results), default=0),
            "flush_s": flush,
            "wire_bytes": {"client_to_server": delta["client"]["shaped_bytes"],
                           "server_to_client": delta["server"]["shaped_bytes"]},
            "wire": delta,
            "monitor": mon,
            "collect": adapter.collect(test),
        }
        if test == "fetch" and all(s["exit"] == 0 for s in results):
            self.log(f"{name}: verifying the fetched tree")
            ph["verify"] = self.verify(adapter, rd, rep)
        ph["flags"] = flags(ph)
        return ph

    def step(self, rd, step_name, argv, cwd, env):
        """Run one timed step under the stall watchdog. The wall clock is the agent's own: GNU time,
        which gives the CPU time and peak memory, writes nothing when the watchdog stops it."""
        timefile = os.path.join(rd, f"{step_name}.time")
        with open(os.path.join(rd, f"{step_name}.log"), "w") as out:
            t0 = time.monotonic()
            # PWD as well as the working directory: P4 resolves relative paths against $PWD.
            proc = subprocess.Popen(["/usr/bin/time", "-v", "-o", timefile, *argv], cwd=cwd, env=dict(env, PWD=cwd),
                                    stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                    start_new_session=True)
            stalled = self.watch(proc, step_name)
            wall = time.monotonic() - t0
        # GNU time exits with the step's status, or 128 and the signal that ended it, and with a
        # signal of its own when the watchdog stops it. Its file says "Exit status: 0" for a step a
        # signal ended.
        result = {"name": step_name, **util.gnu_time(timefile), "wall_s": wall, "exit": proc.returncode}
        if stalled:
            result["stalled"] = stalled
            result["exit"] = result["exit"] or -1
        return result

    def watch(self, proc, step_name):
        """Wait for PROC, checking every 10 s that its step makes progress (see progressed). A step
        with no progress for run.stall_s, or running past run.step_timeout_s, is killed; returns
        why, or None."""
        stall_s, cap_s = self.spec["run"]["stall_s"], self.spec["run"]["step_timeout_s"]
        start = last_progress = time.monotonic()
        prev = last = self.progress(proc.pid)
        while True:
            try:
                proc.wait(timeout=self.poll_s)
                self.beat(None)
                return None
            except subprocess.TimeoutExpired:
                pass
            now, cur = time.monotonic(), self.progress(proc.pid)
            if progressed(prev, last, cur):
                last_progress, prev = now, cur
            self.beat({"step": step_name, "elapsed_s": round(now - start), "since_progress_s": round(now - last_progress),
                       "net_mbit": round((cur["net_bytes"] - last["net_bytes"]) * 8 / 1e6 / max(cur["t"] - last["t"], 1e-3)),
                       "cpu_cores": round((cur["cpu_s"] - last["cpu_s"]) / max(cur["t"] - last["t"], 1e-3), 1)})
            last = cur
            reason = None
            if now - last_progress > stall_s:
                reason = f"no progress for {round(now - last_progress)} s"
            elif now - start > cap_s:
                reason = f"still running after {round(now - start)} s, the step timeout"
            if reason:
                self.log(f"STALLED: {step_name}: {reason}; stopping it")
                left = stop_session(proc, self.kill_grace_s)
                if left:
                    self.log(f"STALLED: {step_name}: processes {left} outlived SIGKILL")
                self.beat(None)
                return reason

    def progress(self, pid):
        """Counters that move while a step works: bytes through the network card, sectors read and
        written on the data disk, and CPU seconds of every process in the step's session."""
        net = sum(int(open(f"/sys/class/net/{self.dev}/statistics/{d}_bytes").read())
                  for d in ("rx", "tx")) if self.dev else 0
        sectors = 0
        for line in open("/proc/diskstats"):
            f = line.split()
            if f[2] == self.disk:
                sectors = int(f[5]) + int(f[9])
        ticks = sum(sum(int(x) for x in fields[11:15]) for _, fields in _session(pid))  # utime, stime, cutime, cstime
        return {"t": time.monotonic(), "net_bytes": net, "disk_sectors": sectors,
                "cpu_s": ticks / os.sysconf("SC_CLK_TCK")}

    def beat(self, step):
        """The heartbeat `bench watch` reads: when the agent last checked in, and its step's progress."""
        util.write_json(os.path.join(self.run_dir, "heartbeat.json"), {"time": time.time(), "step": step})

    def verify(self, adapter, rd, rep):
        out = verify_tree(self.prepared["corpus"], adapter.expected_files, adapter.ctx.work, rd,
                          self.spec["run"]["verify_sha256"], rep)
        self.log("verify: " + ", ".join(f"{k}={v}" for k, v in out.items() if not k.endswith("mismatches")))
        return out


def verify_tree(info, expected_files, work, rd, mode, rep):
    """Compare the tree fetched into WORK with corpus INFO: the file list and every file's size
    always, every file's SHA-256 as MODE (run.verify_sha256) says for repetition REP.
    EXPECTED_FILES maps the corpus's paths to those the tool can hold."""
    sizes = {}
    with open(info["stat"], encoding="utf-8", errors="surrogateescape") as f:
        for line in f:
            path, size, _, _ = line.rstrip("\n").rsplit("\t", 3)  # a name may hold a tab
            sizes[path] = int(size)
    want = expected_files(list(sizes))
    got = []
    for dirpath, dirnames, filenames in os.walk(work):
        if dirpath == work and ".lore" in dirnames:
            dirnames.remove(".lore")
        got += [os.path.relpath(os.path.join(dirpath, f), work) for f in filenames]
    got.sort(key=util.sort_key)
    out = {"expected_files": len(want), "fetched_files": len(got), "files_match": got == want}
    fetched = {p: os.lstat(os.path.join(work, p)).st_size for p in got}
    out["fetched_bytes"] = sum(fetched.values())
    resized = [p for p in want if p in fetched and fetched[p] != sizes[p]]
    out["sizes_match"] = not resized
    out["size_mismatches"] = resized[:20]
    if out["files_match"] and (mode == "all" or (mode == "first" and rep == 1)):
        listing = os.path.join(rd, "expected-files.txt")
        with open(listing, "w", encoding="utf-8", errors="surrogateescape") as f:
            f.writelines(p + "\n" for p in want)
        sums = corpus.sha256_tree(work, listing)
        expected = {}
        for line in open(info["sha256"], encoding="utf-8", errors="surrogateescape"):
            digest, _, path = line.rstrip("\n").partition("  ")
            expected[path] = digest
        bad = [p for p in want if sums.get(p) != expected.get(p)]
        out["sha256_match"] = not bad
        out["sha256_mismatches"] = bad[:20]
    return out


# A step's device-wide traffic counts as progress above this rate, in bytes a second. The rate
# leaves out what moves while a step is stuck: system activity recording, heartbeats and SSH
# keepalives come to tens of kB/s, while a working step moves data at MB/s, or uses CPU.
PROGRESS_BYTES_PER_S = 100_000


def progressed(since, last, cur):
    """Whether a step progressed by sample CUR: its network card or data disk moved more than
    PROGRESS_BYTES_PER_S since sample LAST, or its own processes used a CPU second since SINCE, the
    sample at its last progress. The card and the disk are the host's, so they are judged by their
    rate over one poll; the CPU time is the step's own, so it adds up."""
    seconds = max(cur["t"] - last["t"], 1e-3)
    return ((cur["net_bytes"] - last["net_bytes"]) / seconds > PROGRESS_BYTES_PER_S
            or (cur["disk_sectors"] - last["disk_sectors"]) * 512 / seconds > PROGRESS_BYTES_PER_S
            or cur["cpu_s"] - since["cpu_s"] > 1.0)


def shaping_delay_us(rtt_ms, base_rtt_ms):
    """The delay each host adds to what it sends, so that the round trip is RTT_MS on a link whose
    unshaped round trip is BASE_RTT_MS: half the difference, and none on a link already slower."""
    return max(0, int(round((rtt_ms - (base_rtt_ms or 0)) * 1000 / 2)))


def executable(path, cache, sha256, name):
    """PATH, or a copy of it in CACHE when it isn't executable: a file:// binary is used in place,
    and may belong to someone else."""
    if os.access(path, os.X_OK):
        return path
    copy = os.path.join(cache, sha256, name)
    if os.path.abspath(copy) != os.path.abspath(path):
        os.makedirs(os.path.dirname(copy), exist_ok=True)
        shutil.copyfile(path, copy)
    os.chmod(copy, 0o755)
    return copy


def verified(v):
    """Whether a verification passed: the file list, the sizes, and the SHA-256s where checked."""
    return v["files_match"] and v.get("sizes_match", True) and v.get("sha256_match", True)


def stale_helpers(fingerprints):
    """The roles whose installed bench-helper isn't the harness's copy. Installing the harness
    doesn't install it: it is root's, and only the bootstrap installs it."""
    return [role for role in ("client", "server")
            if fingerprints[role]["helper"]["installed"] != fingerprints[role]["helper"]["harness"]]


def changed_class(before, after):
    """What differs between two hosts' fingerprints, as far as comparing results goes."""
    out = []
    for role in ("client", "server"):
        b, a = before[role]["class"], after[role]["class"]
        out += [f"{role} {k}: {b.get(k)!r} then {a.get(k)!r}" for k in sorted(set(b) | set(a)) if b.get(k) != a.get(k)]
    return out


def _session(sid):
    """(pid, stat fields after the name) of every live process in session SID."""
    out = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            stat = open(f"/proc/{entry}/stat").read()
        except OSError:
            continue
        fields = stat[stat.rindex(")") + 2:].split()
        if int(fields[3]) == sid and fields[0] != "Z":
            out.append((int(entry), fields))
    return out


def stop_session(proc, grace_s=30):
    """Stop everything in the session PROC leads, which a step's processes stay in even when they
    make process groups of their own, though not when they start a session of their own: SIGTERM,
    then SIGKILL to what is left after GRACE_S. Waits for
    the processes, not for PROC: GNU time leads a step and exits on SIGTERM at once, while a child
    that ignores it would run on into the next step. Returns the pids still alive at the end."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid, _ in _session(proc.pid):
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + grace_s
        while True:
            proc.poll()  # reap the leader, which would otherwise stay in the session as a zombie
            left = [pid for pid, _ in _session(proc.pid)]
            if not left or time.monotonic() > deadline:
                break
            time.sleep(0.2)
        if not left:
            break
    try:
        proc.wait(timeout=grace_s)
    except subprocess.TimeoutExpired:
        pass
    return [pid for pid, _ in _session(proc.pid)]


def _delta(after, before):
    if isinstance(after, dict):
        return {k: _delta(v, (before or {}).get(k)) for k, v in after.items()}
    return (after or 0) - (before or 0)


def flags(ph):
    """Why a phase's numbers may not mean what they seem. A flagged phase is kept and shown, and
    left out of trend medians."""
    out = []
    if any(s["exit"] != 0 for s in ph["steps"]):
        out.append("step-failed")
    if any(s.get("stalled") for s in ph["steps"]):
        out.append("stalled")
    v = ph.get("verify")
    if v is not None and not verified(v):
        out.append("verify-failed")
    if any(m.get("cpu_steal_pct", 0) > 1.0 for m in ph["monitor"].values()):
        out.append("steal")
    # Frames the card or the kernel's backlog had no room for. Not rx_dropped, which also counts
    # frames of protocols nothing on the host handles, such as a switch's LLDP.
    if any(w.get("rx_missed", 0) + w.get("softnet_drops", 0) > 0 for w in ph["wire"].values()):
        out.append("nic-drops")
    if any(sum(w.get("ena", {}).values()) > 0 for w in ph["wire"].values()):
        out.append("throttled")
    server = ph["monitor"]["server"]
    if server.get("users") and server["seconds"] and "cpu_user_cores" in server:
        other = server["cpu_user_cores"] - server["users"]["usr_s"] / server["seconds"]
        if other > 0.5:
            out.append("busy-server")
    return out


def main(argv):
    if len(argv) != 2 or argv[0] not in ("prepare", "run"):
        print("usage: bench agent {prepare,run} RUN_DIR", file=sys.stderr)
        return 2
    agent = Agent(argv[1])
    agent.lock()
    end = os.path.join(agent.run_dir, "end.json")
    if os.path.exists(end):
        os.remove(end)
    code, error = 1, None
    try:
        if argv[0] == "prepare":
            agent.prepare()
            code = 0
        else:
            code = agent.run()
        return code
    except BaseException as e:
        error = f"{type(e).__name__}: {e}"
        agent.log(f"FAILED: {error}")
        raise
    finally:
        # How the agent ended, for `bench watch`: a killed agent leaves no file at all.
        util.write_json(end, {"command": argv[0], "exit": code, "error": error, "time": time.time()})
