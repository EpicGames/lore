"""The agent: the watchdog, steps and phases, verification, resuming and how it ends."""
import json
import os
import subprocess
import sys
import time

import pytest

from bench import agent as agentmod
from bench import calibrate, corpus, util
from bench.adapters.lore import Lore
from bench.agent import Agent, changed_class, executable, progressed, verified, verify_tree

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUSY = "import time\nt = time.time()\nwhile time.time() - t < {}: pass"


def alive(pid):
    try:
        state = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[0]
    except FileNotFoundError:
        return False
    return state != "Z"


def no_ops(agent):
    """Host operations that succeed and do nothing, recorded in the list returned."""
    calls = []
    for host in (agent.client, agent.server):
        host.op = lambda name, *args, _role=host.role, **kw: calls.append((_role, name, *args)) or {"seconds": 0.0}
        host.run = lambda *args, **kw: None
    return calls


# ---- the watchdog and steps


def test_the_watchdog_stops_a_step_that_makes_no_progress(tmp_path, make_agent):
    agent = make_agent(stall_s=1)
    t0 = time.monotonic()
    res = agent.step(str(tmp_path), "hang", ["sleep", "60"], str(tmp_path), dict(os.environ))
    elapsed = time.monotonic() - t0
    assert elapsed < 30 and res["stalled"] and res["exit"] != 0
    # GNU time, stopped, writes nothing: its wall clock would be 0 s.
    assert 1 <= res["wall_s"] <= elapsed
    assert "STALLED: hang" in (tmp_path / "run.log").read_text()


def test_the_watchdog_leaves_a_working_step_alone(tmp_path, make_agent):
    agent = make_agent(stall_s=1)
    res = agent.step(str(tmp_path), "busy", [sys.executable, "-c", BUSY.format(3)], str(tmp_path), dict(os.environ))
    assert "stalled" not in res and res["exit"] == 0


def test_the_step_timeout(tmp_path, make_agent):
    agent = make_agent(step_timeout_s=1)
    res = agent.step(str(tmp_path), "long", [sys.executable, "-c", BUSY.format(20)], str(tmp_path), dict(os.environ))
    assert res["stalled"] and res["exit"] != 0


def test_the_watchdog_stops_a_child_that_ignores_sigterm_in_a_group_of_its_own(tmp_path, make_agent):
    # GNU time exits on SIGTERM at once; waiting for it alone would leave this child running into
    # the next step, and so would signalling only the step's process group.
    agent = make_agent(stall_s=1)
    pidfile = tmp_path / "child.pid"
    child = ("import os, signal, sys, time\nos.setpgid(0, 0)\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
             "open(sys.argv[1], 'w').write(str(os.getpid()))\ntime.sleep(120)")
    res = agent.step(str(tmp_path), "stubborn", [sys.executable, "-c", child, str(pidfile)], str(tmp_path),
                     dict(os.environ))
    assert res["stalled"]
    assert not alive(int(pidfile.read_text()))


def test_a_step_a_signal_ended_has_failed(tmp_path, make_agent):
    # GNU time writes "Exit status: 0" for a command a signal ended, as for one the OOM killer stops.
    agent = make_agent()
    res = agent.step(str(tmp_path), "killed", ["sh", "-c", "kill -KILL $$"], str(tmp_path), dict(os.environ))
    assert res["exit"] == 128 + 9
    assert agentmod.flags({"steps": [res], "wire": {}, "monitor": {"client": {}, "server": {}}}) == ["step-failed"]


def test_gnu_time_reads_the_signal_before_the_status(tmp_path):
    path = tmp_path / "time"
    path.write_text("\tCommand terminated by signal 9\n\tExit status: 0\n")
    assert util.gnu_time(str(path))["exit"] == 137


def sample(t, net=0, sectors=0, cpu=0.0):
    return {"t": t, "net_bytes": net, "disk_sectors": sectors, "cpu_s": cpu}


def test_a_background_trickle_is_not_progress():
    # System activity recording writes some 17 kB/s to the data disk: over a 10-minute stall limit
    # it adds up to megabytes, which a total would take for progress.
    since = sample(0)
    for poll in range(1, 61):
        last, cur = sample(10 * (poll - 1), sectors=340 * (poll - 1)), sample(10 * poll, sectors=340 * poll)
        assert not progressed(since, last, cur)


def test_progress_is_traffic_at_a_working_rate_or_the_steps_own_cpu():
    since, last = sample(0), sample(0)
    assert progressed(since, last, sample(10, net=2_000_000))
    assert progressed(since, last, sample(10, sectors=4_000))
    assert not progressed(since, last, sample(10, cpu=0.5))
    # The step's CPU time is its own, so it adds up from the last progress.
    assert progressed(since, sample(10, cpu=0.6), sample(20, cpu=1.2))


def test_one_agent_at_a_time(tmp_path, make_agent):
    first, second = make_agent(), Agent(str(tmp_path))
    first.lock()
    with pytest.raises(RuntimeError):
        second.lock()


# ---- phases


class Steps:
    """An adapter whose import is the given steps."""
    server_pidfile = "/nonexistent"

    def __init__(self, steps):
        self.steps = steps

    def setup_import(self):
        pass

    def import_steps(self):
        return self.steps


def test_a_step_that_raises_stops_the_monitors(tmp_path, make_agent):
    agent = make_agent(settle_s=0)
    calls = no_ops(agent)
    with pytest.raises(FileNotFoundError):
        agent.phase("p/x-r1", "import", Steps([("missing", ["true"], "/nonexistent/dir", dict(os.environ))]),
                    str(tmp_path), str(tmp_path / "server"), 1)
    assert [c[:2] for c in calls if c[1] == "monitor-stop"] == [("client", "monitor-stop"), ("server", "monitor-stop")]


def test_a_monitor_that_fails_to_start_stops_the_one_that_started(tmp_path, make_agent):
    agent = make_agent(settle_s=0)
    calls = no_ops(agent)

    def server_op(name, *args, **kw):
        if name == "monitor-start":
            raise RuntimeError("no pidstat")
        return {"seconds": 0.0}

    agent.server.op = server_op
    with pytest.raises(RuntimeError):
        agent.phase("p/x-r1", "import", Steps([]), str(tmp_path), str(tmp_path / "server"), 1)
    assert ("client", "monitor-stop") in [c[:2] for c in calls]


# ---- profiles, calibration and preparing


def test_a_failed_calibration_stops_only_its_profile(make_agent):
    agent = make_agent()
    agent.spec["profile"] = [{"name": "p1"}, {"name": "p2"}]
    agent.spec["run"].update(repetitions=1, order="fixed", tool_order=["lore"])
    ran = []
    agent.prepare = lambda: None
    agent.shape = lambda profile: None
    agent.calibrate = lambda profile: profile["name"] == "p2"
    agent.done = lambda profile, tool, rep: False
    agent.run_one = lambda profile, rep, position, tool: ran.append(profile["name"])
    no_ops(agent)
    assert agent.run() == 1
    assert ran == ["p2"]


def test_the_log_says_when_the_watchdog_sees_cpu_only(tmp_path, make_agent):
    agent = make_agent()  # not on a prepared host: no network card or data disk to read
    agent.spec["profile"] = []
    agent.spec["run"].update(repetitions=1, order="fixed", tool_order=[])
    agent.prepare = lambda: None
    no_ops(agent)
    assert agent.run() == 0
    assert "watchdog: can't read the network card or data disk" in (tmp_path / "run.log").read_text()


def test_calibration_runs_again_when_an_agent_resumes(tmp_path, make_agent, monkeypatch):
    agent = make_agent(calibration={"enforce": True})
    agent.prepared = {"base_rtt_ms": {}}
    (tmp_path / "p").mkdir()
    util.write_json(str(tmp_path / "p" / "calibration.json"), {"ok": True})
    calls = []
    monkeypatch.setattr(agentmod.calibrate, "run", lambda *args: calls.append(args) or ({}, []))
    assert agent.calibrate({"name": "p"})
    assert len(calls) == 1


def test_an_error_in_calibration_stops_only_its_profile(tmp_path, make_agent, monkeypatch):
    agent = make_agent(calibration={"enforce": False})
    agent.prepared = {"base_rtt_ms": {}}

    def fails(*args):
        raise ValueError("iperf3 wrote half a JSON document")

    monkeypatch.setattr(agentmod.calibrate, "run", fails)
    assert not agent.calibrate({"name": "p"})
    result = util.read_json(str(tmp_path / "p" / "calibration.json"))
    assert not result["ok"] and result["problems"]


def test_the_unshaped_round_trip_is_measured_on_iperf3_not_sshd(make_agent, monkeypatch):
    # OpenSSH 9.8 and later refuse a source whose connections close without authenticating.
    agent = make_agent()
    calls = no_ops(agent)
    ports = []
    monkeypatch.setattr(calibrate, "connect_rtts", lambda address, port, count, **kw: ports.append(port) or [1.0])
    agent.base_rtt()
    assert ports == [calibrate.IPERF_PORT]
    assert [c[1] for c in calls] == ["iperf-start", "iperf-stop"]


def test_the_shaper_adds_only_what_the_unshaped_link_lacks():
    assert agentmod.shaping_delay_us(5, 1.12) == 1940
    assert agentmod.shaping_delay_us(100, 1.12) == 49440
    assert agentmod.shaping_delay_us(30, None) == 15000
    # A link already slower than the profile gets no delay at all.
    assert agentmod.shaping_delay_us(0.5, 1.12) == 0


def round_trips(monkeypatch, *medians):
    """Connection times whose medians are MEDIANS, one measurement after the other."""
    left = iter(medians)
    monkeypatch.setattr(calibrate, "connect_rtts", lambda *args, **kw: [next(left)] * 20)


def test_each_profile_is_shaped_on_the_unshaped_round_trip_measured_for_it(tmp_path, make_agent, monkeypatch):
    # A VM's network path adds tenths of a millisecond, and how much drifts over a run: the unshaped
    # round trip is measured again for each profile, with the link unshaped. The shaped link is
    # slower than that plus the delay, so the delay is corrected by what the shaped round trip
    # misses, and the calibration records the measurements and the delay that stood.
    agent = make_agent(calibration={"enforce": True})
    agent.prepared = {"base_rtt_ms": {"median": 0.5}}
    calls = no_ops(agent)
    round_trips(monkeypatch, 1.2, 5.6, 5.0)
    agent.shape({"name": "p", "rtt_ms": 5, "rate_mbit": 984})
    names = [c[:2] for c in calls]
    assert names.index(("client", "unshape")) < names.index(("server", "iperf-start")) < names.index(("client", "shape"))
    assert [c[3] for c in calls if c[1] == "shape"] == [1900, 1900, 1600, 1600]
    bases = []
    monkeypatch.setattr(agentmod.calibrate, "run", lambda *args: bases.append(args[-1]) or ({"profile": "p"}, []))
    assert agent.calibrate({"name": "p", "rtt_ms": 5})
    assert bases == [1.2]
    result = util.read_json(str(tmp_path / "p" / "calibration.json"))
    assert result["base_rtt_ms"]["median"] == 1.2 and result["delay_us_each_way"] == 1600
    assert result["shaped_rtt_ms"]["median"] == 5.0


def test_a_shaped_round_trip_close_enough_is_left_as_it_is(make_agent, monkeypatch):
    agent = make_agent()
    calls = no_ops(agent)
    round_trips(monkeypatch, 1.2, 5.05)
    agent.shape({"name": "p", "rtt_ms": 5})
    assert [c[3] for c in calls if c[1] == "shape"] == [1900, 1900]


def test_the_calibration_expects_the_profiles_round_trip_not_more():
    tolerance = {"rtt_tolerance_ms": 1.0, "rtt_tolerance_rel": 0.05, "tcp_min_fraction": 0.9}
    idle = lambda ms: {"rtt_idle_ms": {"median": ms}}
    assert calibrate.gate(idle(5.3), {"rtt_ms": 5}, tolerance, 1.2) == []
    # The profile's round trip plus the unshaped one is what the tools would have seen before.
    assert calibrate.gate(idle(6.4), {"rtt_ms": 5}, tolerance, 1.2)
    # A link already slower than the profile is judged against its unshaped round trip.
    assert calibrate.gate(idle(1.3), {"rtt_ms": 0.5}, tolerance, 1.2) == []


FINGERPRINT = {"cpus": 16, "cpu_model": "x", "mem_bytes": 2**35, "kernel": "7.0", "virt": "kvm",
               "disk": {"fstype": "xfs", "device": "vdb"}, "helper": {"installed": "h1", "harness": "h1"},
               "class": {"kernel": "7.0"}}


def resumable(tmp_path, agent, fingerprint):
    """AGENT on a run prepared before, whose hosts now report FINGERPRINT; returns the operations."""
    calls = []

    def op(role):
        def run(name, *args, **kw):
            calls.append((role, name))
            return dict(fingerprint()) if name == "fingerprint" else {}
        return run

    agent.client.op, agent.server.op = op("client"), op("server")
    agent.spec.update(host={"sysctl": {"vm.dirty_ratio": "20"}}, corpus={"sha256": "a" * 64, "root": ""})
    (tmp_path / "prepare").mkdir()
    util.write_json(str(tmp_path / "prepare" / "prepared.json"),
                    {"fingerprint": {"client": FINGERPRINT, "server": FINGERPRINT}, "sysctl": {}, "base_rtt_ms": {},
                     "tools": {}, "corpus": {"id": "c1"}})
    return calls


def test_resuming_applies_the_settings_again_and_checks_the_hosts(tmp_path, make_agent, monkeypatch):
    agent = make_agent()
    current = {"fp": FINGERPRINT}
    calls = resumable(tmp_path, agent, lambda: current["fp"])
    agent.resolve_tools = lambda: {}
    monkeypatch.setattr(corpus, "prepare", lambda c, data, log: {"id": "c1"})
    agent.prepare()
    assert ("client", "sysctl") in calls and ("server", "sysctl") in calls
    current["fp"] = dict(FINGERPRINT, **{"class": {"kernel": "7.1"}})
    with pytest.raises(RuntimeError):
        agent.prepare()


def test_resuming_fetches_what_is_gone_and_needs_the_same_corpus(tmp_path, make_agent, monkeypatch):
    # Instance store is erased when an instance stops: the hosts are the same kind, the files gone.
    agent = make_agent()
    resumable(tmp_path, agent, lambda: FINGERPRINT)
    fetched = []
    agent.resolve_tools = lambda: fetched.append("tools") or {"lore": {}}
    monkeypatch.setattr(corpus, "prepare", lambda c, data, log: fetched.append("corpus") or {"id": "c1"})
    assert agent.prepare()["tools"] == {"lore": {}}
    assert fetched == ["tools", "corpus"]
    monkeypatch.setattr(corpus, "prepare", lambda c, data, log: {"id": "c2"})
    with pytest.raises(RuntimeError):
        agent.prepare()


def test_a_host_whose_helper_isnt_the_harnesss_is_refused(make_agent):
    # Installing the harness leaves the root-owned helper as it was: an older one shapes another way.
    agent = make_agent()
    agent.spec.update(host={"sysctl": {}})
    stale = dict(FINGERPRINT, helper={"installed": "h0", "harness": "h1"})
    agent.client.op = lambda name, *args, **kw: dict(FINGERPRINT) if name == "fingerprint" else {}
    agent.server.op = lambda name, *args, **kw: dict(stale) if name == "fingerprint" else {}
    agent.base_rtt = lambda: pytest.fail("a run on a stale helper went on to measure")
    with pytest.raises(RuntimeError, match="installed on the server isn't this harness's"):
        agent.prepare()


def test_changed_class():
    before = {role: {"class": {"cpus": 16, "kernel": "7.0"}} for role in ("client", "server")}
    after = json.loads(json.dumps(before))
    after["server"]["class"]["kernel"] = "7.1"
    assert changed_class(before, before) == []
    assert changed_class(before, after) == ["server kernel: '7.0' then '7.1'"]


def test_a_binary_that_isnt_executable_is_copied_not_changed(tmp_path):
    src = tmp_path / "p4"
    src.write_bytes(b"#!/bin/sh\n")
    src.chmod(0o644)
    copy = executable(str(src), str(tmp_path / "cache"), "a" * 64, "p4")
    assert copy != str(src) and os.access(copy, os.X_OK)
    assert src.stat().st_mode & 0o777 == 0o644
    src.chmod(0o755)
    assert executable(str(src), str(tmp_path / "cache"), "a" * 64, "p4") == str(src)


# ---- resuming and how a run ends


def hold_lock(rd):
    """A process holding RD's agent lock, as a running agent does."""
    code = ("import fcntl, sys, time\nf = open(sys.argv[1], 'a')\nfcntl.flock(f, fcntl.LOCK_EX)\n"
            "print('held', flush=True)\ntime.sleep(30)")
    proc = subprocess.Popen([sys.executable, "-c", code, str(rd / "agent.lock")], stdout=subprocess.PIPE, text=True)
    assert proc.stdout.readline().strip() == "held"
    return proc


def test_a_run_resumes_only_with_its_spec_and_not_beside_its_agent(tmp_path):
    from bench.__main__ import check_resume
    from bench.remote import Host, HostError
    client = Host("client", "localhost", "me", local=True)
    rd = tmp_path / "run"
    rd.mkdir()
    spec = {"_sha256": "a" * 64}
    assert check_resume(client, str(rd), spec) is None  # a new run
    (rd / "spec.json").write_text(json.dumps(spec))
    assert check_resume(client, str(rd), spec) == spec
    with pytest.raises(HostError):
        check_resume(client, str(rd), {"_sha256": "b" * 64})
    agent = hold_lock(rd)  # an agent in the foreground writes no agent.pid
    try:
        with pytest.raises(HostError):
            check_resume(client, str(rd), spec)
    finally:
        agent.kill()
        agent.wait()


def test_a_failed_run_is_repeated_on_resume(tmp_path, make_agent):
    agent = make_agent()
    profile = {"name": "p"}
    assert not agent.done(profile, "p4", 1)
    rd = tmp_path / "p" / "p4-r1"
    rd.mkdir(parents=True)
    (rd / "result.json").write_text(json.dumps({"phases": {"import": {"flags": ["step-failed"]}}}))
    assert not agent.done(profile, "p4", 1)
    (rd / "result.json").write_text(json.dumps({"phases": {"import": {"flags": ["steal"]}, "fetch": {"flags": []}}}))
    assert agent.done(profile, "p4", 1)


def test_the_agent_records_how_it_ended(tmp_path, make_agent):
    make_agent()  # a server nothing listens for, so preparing fails at once
    res = subprocess.run([sys.executable, "-m", "bench", "agent", "prepare", str(tmp_path)], cwd=HERE,
                         capture_output=True, text=True, timeout=120)
    assert res.returncode != 0
    end = util.read_json(str(tmp_path / "end.json"))
    assert end["exit"] == 1 and end["error"]


def test_watch_tells_a_finished_run_from_a_stopped_one():
    from bench.__main__ import outcome
    assert outcome({"exit": 0, "error": None})[1] == 0
    assert outcome({"exit": 1, "error": None})[1] == 1  # a profile stopped by its calibration
    assert outcome({"exit": 1, "error": "RuntimeError: no"})[1] == 1
    assert outcome(None)[1] == 1  # killed


def test_watch_warns_of_a_stuck_step_and_of_silence_outside_steps():
    from bench.__main__ import stall_notes
    step = {"step": "lore-clone", "since_progress_s": 200}
    assert len(stall_notes(True, step, beat_age=10, line_age=0, stall=180)) == 1
    assert stall_notes(True, dict(step, since_progress_s=60), beat_age=10, line_age=0, stall=180) == []
    assert len(stall_notes(True, dict(step, since_progress_s=60), beat_age=400, line_age=0, stall=180)) == 1
    # Outside a step heartbeats don't come, so their age is no warning; silence in the log is.
    assert stall_notes(True, None, beat_age=4000, line_age=300, stall=180) == []
    assert len(stall_notes(True, None, beat_age=4000, line_age=700, stall=180)) == 1
    assert stall_notes(False, None, beat_age=4000, line_age=4000, stall=180) == []


def test_a_command_that_outlives_its_timeout_is_a_host_error():
    from bench.remote import Host, HostError
    with pytest.raises(HostError):
        Host("client", "localhost", "me", local=True).run("sleep 5", timeout=1)


# ---- the corpus and verification


def make_tree(path, files):
    for name, data in files.items():
        full = path / name
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_bytes(data)
    return str(path)


def make_corpus(tmp_path, files):
    root, base = make_tree(tmp_path / "corpus", files), tmp_path / "base"
    base.mkdir()
    return corpus.describe(root, str(base))


def test_verification_checks_every_files_size(tmp_path):
    info = make_corpus(tmp_path, {"a.txt": b"hello", "d/b.bin": b"x" * 100})
    work = make_tree(tmp_path / "work", {"a.txt": b"hello", "d/b.bin": b"x" * 99})
    out = verify_tree(info, list, work, str(tmp_path), "first", 2)  # no SHA-256 after the first repetition
    assert out["files_match"] and "sha256_match" not in out
    assert out["size_mismatches"] == ["d/b.bin"] and not verified(out)


def test_verification_checks_sha256_on_the_first_repetition(tmp_path):
    info = make_corpus(tmp_path, {"a.txt": b"hello", "b.txt": b"same"})
    work = make_tree(tmp_path / "work", {"a.txt": b"jello", "b.txt": b"same"})
    out = verify_tree(info, list, work, str(tmp_path), "first", 1)
    assert out["sizes_match"] and out["sha256_mismatches"] == ["a.txt"] and not verified(out)


def test_names_with_a_tab_or_a_backslash(tmp_path):
    # sha256sum escapes a name holding a backslash, and stat.tsv separates its fields with tabs.
    files = {"tab\tname": b"1", "back\\slash": b"22"}
    info = make_corpus(tmp_path, files)
    out = verify_tree(info, list, make_tree(tmp_path / "work", files), str(tmp_path), "all", 1)
    assert verified(out) and out["fetched_files"] == 2


def test_lore_fetches_one_of_paths_that_differ_only_in_case(tmp_path):
    info = make_corpus(tmp_path, {"Docs/README": b"1", "Docs/Readme": b"2", "x": b"3"})
    work = make_tree(tmp_path / "work", {"Docs/README": b"1", "x": b"3"})
    out = verify_tree(info, lambda paths: Lore.expected_files(None, paths), work, str(tmp_path), "all", 1)
    assert verified(out) and out["expected_files"] == 2


def test_restore_puts_back_modes_and_times_and_refuses_a_resized_file(tmp_path):
    info = make_corpus(tmp_path, {"a": b"1", "b": b"22"})
    root = tmp_path / "corpus"
    mode, mtime = os.lstat(root / "a").st_mode, os.lstat(root / "a").st_mtime_ns
    os.chmod(root / "a", 0o444)
    os.utime(root / "a", ns=(0, 0))
    (root / ".lore").mkdir()
    assert corpus.restore(info) == {"restored": 2}
    st = os.lstat(root / "a")
    assert (st.st_mode, st.st_mtime_ns) == (mode, mtime) and not (root / ".lore").exists()
    (root / "b").write_bytes(b"333")
    with pytest.raises(corpus.CorpusError):
        corpus.restore(info)


def test_the_corpus_cache_tells_roots_apart(tmp_path):
    src = tmp_path / "src"
    make_tree(src, {"Engine/a": b"1", "Engine/Source/b": b"2"})
    archive = tmp_path / "corpus.tar"
    subprocess.run(["tar", "-C", str(src), "-cf", str(archive), "Engine"], check=True)
    c = {"url": f"file://{archive}", "sha256": util.sha256_file(str(archive)), "format": "auto"}
    data = tmp_path / "data"
    whole = corpus.prepare(dict(c, root="Engine"), str(data), lambda m: None)
    source = corpus.prepare(dict(c, root="Engine/Source"), str(data), lambda m: None)
    assert (whole["files"], source["files"]) == (2, 1)
