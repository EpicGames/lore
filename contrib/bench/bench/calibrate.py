"""Calibration: measure the shaped link and both data disks before a profile's runs, and stop the
profile if the link isn't what the spec asks for.

Round-trip time is measured as the time a TCP connection to the server's iperf3 port takes to open,
one round trip through both shapers. Ping can't stand in for it: where the server sits behind NAT,
as a VM does, its host answers pings itself. Nor can sshd: OpenSSH 9.8 and later refuse a source
for a while once its connections have closed without authenticating for 15 s' worth of penalties,
a second each by default.
"""
import json
import socket
import statistics
import subprocess
import threading
import time

IPERF_PORT = 5202


def connect_rtts(address, port, count, interval=0.2):
    out = []
    for _ in range(count):
        t0 = time.monotonic()
        with socket.create_connection((address, port), timeout=5):
            out.append((time.monotonic() - t0) * 1000)
        time.sleep(interval)
    return out


def iperf(address, args, seconds):
    try:
        res = subprocess.run(["iperf3", "-c", address, "-p", str(IPERF_PORT), "-t", str(seconds), "-O", "2", "-J",
                              "--connect-timeout", "10000", *args], capture_output=True, text=True, timeout=seconds + 60)
    except subprocess.TimeoutExpired:
        return {"error": f"iperf3 still running {seconds + 60} s into a {seconds} s test"}
    data = json.loads(res.stdout or "{}")
    if "error" in data or res.returncode != 0:
        return {"error": data.get("error", res.stderr.strip()[-500:])}
    end = data["end"]
    if "sum_received" in end:
        return {"mbit": end["sum_received"]["bits_per_second"] / 1e6,
                "retransmits": end.get("sum_sent", {}).get("retransmits")}
    s = end["sum"]
    return {"mbit": s["bits_per_second"] / 1e6, "lost_pct": s.get("lost_percent")}


def stats(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values)} if values else {}


def run(client, server, server_addr, profile, tolerance, log, base_rtt_ms=None):
    """Calibrate the link from the client to SERVER_ADDR, with PROFILE's shaping already on, and the
    data disks of CLIENT and SERVER (Hosts). Returns (result, problems): an empty PROBLEMS list means
    the profile may run."""
    server.op("iperf-start", IPERF_PORT)
    try:
        result = {"profile": profile["name"]}
        result["rtt_idle_ms"] = stats(connect_rtts(server_addr, IPERF_PORT, 20))
        log(f"idle round trip {result['rtt_idle_ms']['median']:.1f} ms")
        tcp = {}
        for label, args in (("up_1", ["-P", "1"]), ("up_4", ["-P", "4"]), ("down_1", ["-P", "1", "-R"]),
                            ("down_4", ["-P", "4", "-R"])):
            tcp[label] = iperf(server_addr, args, 10)
        result["tcp"] = tcp
        log("TCP " + ", ".join(f"{k} {v.get('mbit', 0):.0f} Mbit/s" for k, v in tcp.items()))
        loaded = []
        t = threading.Thread(target=lambda: iperf(server_addr, ["-P", "4", "-R"], 12))
        t.start()
        time.sleep(5)
        loaded = connect_rtts(server_addr, IPERF_PORT, 12, interval=0.3)  # iperf3 accepts them while busy
        t.join()
        result["rtt_loaded_ms"] = stats(loaded)
        log(f"round trip under a 4-stream download {result['rtt_loaded_ms']['median']:.1f} ms "
            f"(max {result['rtt_loaded_ms']['max']:.1f})")
        if profile.get("rate_mbit"):
            per_stream = f"{profile['rate_mbit'] * 0.92 / 4:.0f}M"
            result["udp"] = {
                "up": iperf(server_addr, ["-u", "-P", "4", "-b", per_stream, "-l", "1400"], 5),
                "down": iperf(server_addr, ["-u", "-P", "4", "-b", per_stream, "-l", "1400", "-R"], 5),
            }
            log("UDP at 92% of the rate: " + ", ".join(
                f"{k} {v.get('mbit', 0):.0f} Mbit/s, {v.get('lost_pct') or 0:.2f}% lost" for k, v in result["udp"].items()))
    finally:
        server.op("iperf-stop", IPERF_PORT)
    if tolerance["disk"]:
        result["disk"] = {}
        for host in (client, server):
            log(f"disk: measuring the {host.role}'s")
            result["disk"][host.role] = host.op("fio", host.data, timeout=1800)
        log("disks: " + "; ".join(f"{role} write {d['seq_write_mbs']:.0f}, read {d['seq_read_mbs']:.0f}, "
                                  f"small files {d['small_write_mbs']:.0f} MB/s" for role, d in result["disk"].items()))
    return result, gate(result, profile, tolerance, base_rtt_ms)


def gate(result, profile, tolerance, base_rtt_ms):
    """What keeps PROFILE from running on the link RESULT measured, if anything. The idle round trip
    must be the profile's, which the shaper reaches by adding what the unshaped link (BASE_RTT_MS)
    lacks of it, or the unshaped one on a link already slower than the profile."""
    problems = []
    want = max(profile["rtt_ms"], base_rtt_ms or 0)
    got = result["rtt_idle_ms"]["median"]
    allowed = max(tolerance["rtt_tolerance_ms"], tolerance["rtt_tolerance_rel"] * profile["rtt_ms"])
    if abs(got - want) > allowed:
        problems.append(f"idle round trip {got:.2f} ms, expected {want:.2f} ± {allowed:.2f} ms")
    if profile.get("rate_mbit"):
        floor = tolerance["tcp_min_fraction"] * profile["rate_mbit"]
        for label in ("up_4", "down_4"):
            mbit = result["tcp"][label].get("mbit", 0)
            if mbit < floor:
                problems.append(f"TCP {label} {mbit:.0f} Mbit/s, below {floor:.0f} "
                                f"({tolerance['tcp_min_fraction']:.0%} of {profile['rate_mbit']})")
    return problems
