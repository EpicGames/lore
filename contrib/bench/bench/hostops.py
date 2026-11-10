"""Host operations: what the agent asks of a host, run on that host as
`python3 -m bench hostops NAME ARGS...`. Each prints one JSON value. Anything needing root goes
through bench-helper, which bootstrap.sh installs as the benchmark user's only root command.
"""
import getpass
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import urllib.request

from . import spec as specmod
from . import util

HELPER_PATH = "/usr/local/sbin/bench-helper"
HELPER = ["sudo", "-n", HELPER_PATH]
CONF = "/etc/bench/helper.conf"
# The harness's own copy, which the bootstrap installs as HELPER_PATH.
HARNESS_HELPER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "host", "bench-helper")


def conf():
    out = {}
    for line in open(CONF):
        key, _, value = line.strip().partition("=")
        if key:
            out[key] = value
    return out


def sh(argv, check=True, **kw):
    res = subprocess.run(argv, text=True, capture_output=True, **kw)
    if check and res.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, argv))} exited {res.returncode}: {res.stderr.strip()[-2000:]}")
    return res.stdout


def helper(*args):
    return sh([*HELPER, *map(str, args)])


# ---- host description


def fingerprint(data):
    """What this host is: hardware, kernel, network card, data disk and settings. "class" is the
    part that decides whether two results are comparable: it goes into the series key."""
    cpuinfo = open("/proc/cpuinfo").read()
    meminfo = dict(re.findall(r"^(\w+):\s+(\d+)", open("/proc/meminfo").read(), re.M))
    dev = conf()["DEV"]
    source, fstype, options = sh(["findmnt", "-no", "SOURCE,FSTYPE,OPTIONS", "--target", data]).split()
    disk = disk_name(data)
    model = _read(f"/sys/block/{disk}/device/model").strip() if disk else ""
    ethtool_i = dict(re.findall(r"^([\w-]+): (.*)$", sh(["ethtool", "-i", dev], check=False), re.M))
    offloads = dict(re.findall(r"^([\w-]+): (on|off)", sh(["ethtool", "-k", dev], check=False), re.M))
    fp = {
        "hostname": os.uname().nodename,
        "cpu_model": _first(r"^model name\s*: (.*)$", cpuinfo),
        "microcode": _first(r"^microcode\s*: (.*)$", cpuinfo),
        "cpus": os.cpu_count(),
        "mem_bytes": int(meminfo.get("MemTotal", 0)) * 1024,
        "kernel": os.uname().release,
        "kernel_version": os.uname().version,
        "cmdline": _read("/proc/cmdline").strip(),
        "os": _first(r'^PRETTY_NAME="?([^"\n]*)', _read("/etc/os-release")),
        "virt": sh(["systemd-detect-virt"], check=False).strip() or "none",
        "clocksource": _read("/sys/devices/system/clocksource/clocksource0/current_clocksource").strip(),
        "nic": {"dev": dev, "driver": ethtool_i.get("driver", ""), "firmware": ethtool_i.get("firmware-version", ""),
                "mtu": int(_read(f"/sys/class/net/{dev}/mtu") or 0),
                "speed": _read(f"/sys/class/net/{dev}/speed").strip(),
                "offloads": {k: offloads.get(k) for k in ("tcp-segmentation-offload", "generic-segmentation-offload",
                                                          "generic-receive-offload", "large-receive-offload")}},
        "disk": {"source": source, "device": disk, "model": model, "fstype": fstype, "options": options,
                 "rotational": _read(f"/sys/block/{disk}/queue/rotational").strip() if disk else "",
                 "size_bytes": int(_read(f"/sys/block/{disk}/size") or 0) * 512 if disk else 0},
        "sysctl": sysctls(),
        "helper": {"installed": _sha256(HELPER_PATH), "harness": _sha256(HARNESS_HELPER)},
        "tools": {name: _version(argv) for name, argv in
                  (("iperf3", ["iperf3", "--version"]), ("fio", ["fio", "--version"]), ("tc", ["tc", "-V"]))},
    }
    fp["class"] = {
        "cpu_model": fp["cpu_model"], "cpus": fp["cpus"], "mem_gib": round(fp["mem_bytes"] / 2**30),
        "kernel": fp["kernel"], "virt": fp["virt"], "nic_driver": fp["nic"]["driver"], "mtu": fp["nic"]["mtu"],
        "disk_model": model, "fstype": fstype, "sysctl": fp["sysctl"],
    }
    return fp


def disk_name(data):
    """The block device under DATA, as sadf names it: the whole disk for a partition."""
    source = sh(["findmnt", "-no", "SOURCE", "--target", data]).strip()
    name = os.path.basename(os.path.realpath(source))
    parent = sh(["lsblk", "-no", "PKNAME", source], check=False).strip().splitlines()
    return parent[0] if parent and parent[0] else name


def sysctls():
    return {k: " ".join(_read("/proc/sys/" + k.replace(".", "/")).split()) for k in specmod.SYSCTL_KEYS}


def apply_sysctl(settings):
    """Apply SETTINGS (key -> value, or "inherit" to leave a key as it is) and return every managed
    setting's effective value."""
    for key, value in settings.items():
        if value != "inherit":
            helper("sysctl", key, value)
    return sysctls()


# ---- network


def shape(peer, delay_us, rate_mbit, queue_bytes, loss_ppm, jitter_us):
    helper("shape", peer, delay_us, rate_mbit, queue_bytes, loss_ppm, jitter_us)
    return {"qdisc": sh(["tc", "qdisc", "show", "dev", conf()["DEV"]]).strip().splitlines()}


def unshape():
    helper("unshape")
    return {}


def wire():
    """Counters for the link: bytes this host sent through its shaper (headers included), what
    the card and the kernel dropped on receipt, TCP retransmits, UDP drops, and on AWS the ENA
    allowance counters."""
    dev = conf()["DEV"]
    qdiscs = {q["kind"]: q for q in json.loads(sh(["tc", "-s", "-j", "qdisc", "show", "dev", dev]) or "[]")}
    shaped = qdiscs.get("tbf") or qdiscs.get("netem") or {}
    stat = lambda name: int(_read(f"/sys/class/net/{dev}/statistics/{name}").strip() or 0)
    snmp = _snmp()
    out = {
        "shaped_bytes": shaped.get("bytes", 0),
        "shaper_drops": shaped.get("drops", 0),
        # Frames the card had no room for, and those the kernel's backlog dropped: drops that cost
        # the link. rx_dropped is kept apart, as it also counts frames of protocols nothing handles.
        "rx_missed": stat("rx_missed_errors") + stat("rx_fifo_errors") + stat("rx_over_errors"),
        "softnet_drops": softnet_drops(_read("/proc/net/softnet_stat")),
        "rx_dropped": stat("rx_dropped"),
        "rx_errors": stat("rx_errors"),
        "tcp_retrans_segs": snmp.get(("Tcp", "RetransSegs"), 0),
        **udp_counters(snmp),
    }
    stats = sh(["ethtool", "-S", dev], check=False)
    for key in ("bw_in_allowance_exceeded", "bw_out_allowance_exceeded", "pps_allowance_exceeded",
                "conntrack_allowance_exceeded"):
        m = re.search(rf"^\s*{key}: (\d+)", stats, re.M)
        if m:
            out.setdefault("ena", {})[key] = int(m.group(1))
    return out


def softnet_drops(text):
    """Packets the kernel dropped because a CPU's receive backlog was full: the second column of
    /proc/net/softnet_stat, one hexadecimal row per CPU."""
    return sum(int(row.split()[1], 16) for row in text.splitlines() if row.strip())


def udp_counters(snmp):
    """The UDP datagrams this host dropped, from /proc/net/snmp's counters (SNMP). Each counter is
    kept on its own: Linux charges a datagram dropped for a full socket receive buffer to both
    RcvbufErrors and InErrors, so their sum counts it twice."""
    return {
        "udp_in_errors": snmp.get(("Udp", "InErrors"), 0),  # every receive drop, the next included
        "udp_rcvbuf_errors": snmp.get(("Udp", "RcvbufErrors"), 0),  # the socket's receive buffer was full
        "udp_sndbuf_errors": snmp.get(("Udp", "SndbufErrors"), 0),
    }


def iperf_start(port):
    iperf_stop(port)
    sh(["iperf3", "-s", "-p", str(port), "-D", "--pidfile", f"/tmp/bench-iperf-{port}.pid"])
    return {}


def iperf_stop(port):
    _kill_pidfile(f"/tmp/bench-iperf-{port}.pid", "iperf3")
    return {}


# ---- disks and caches


def trim():
    helper("trim")
    return {}


def dropcaches():
    helper("dropcaches")
    return {}


def flush():
    t0 = time.monotonic()
    os.sync()
    return {"seconds": time.monotonic() - t0}


def fio(data):
    """Sequential write and read with direct I/O, and a write of many small files like a tree."""
    d = os.path.join(data, "fio-calibration")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(os.path.join(d, "small"))
    common = ["--output-format=json", "--ioengine=libaio", "--group_reporting"]

    def bw(args, rw):
        out = json.loads(sh(["fio", *common, *args]))
        return out["jobs"][0][rw]["bw_bytes"] / 1e6

    seq = os.path.join(d, "seq")
    result = {
        "seq_write_mbs": bw(["--name=seqw", f"--filename={seq}", "--size=4G", "--bs=1M", "--rw=write",
                             "--direct=1", "--iodepth=16", "--end_fsync=1"], "write"),
        "seq_read_mbs": bw(["--name=seqr", f"--filename={seq}", "--size=4G", "--bs=1M", "--rw=read",
                            "--direct=1", "--iodepth=16"], "read"),
        "small_write_mbs": bw(["--name=small", f"--directory={d}/small", "--nrfiles=1024", "--openfiles=32",
                               "--file_service_type=sequential", "--filesize=64k", "--bs=64k", "--rw=write",
                               "--numjobs=8", "--fallocate=none", "--create_on_open=1", "--end_fsync=1"], "write"),
    }
    shutil.rmtree(d, ignore_errors=True)
    return result


def du(path):
    if not os.path.exists(path):
        return {"bytes": 0}
    return {"bytes": int(sh(["du", "-s", "--block-size=1", path]).split()[0])}


def have(path, sha256):
    """Whether PATH exists with content SHA256. A marker beside it, stamped with its size and mtime,
    saves hashing it again."""
    marker = f"{path}.sha256"
    try:
        st = os.stat(path)
    except FileNotFoundError:
        return {"ok": False}
    stamp = f"{sha256} {st.st_size} {st.st_mtime_ns}"
    if _read(marker).strip() == stamp:
        return {"ok": True}
    if util.sha256_file(path) != sha256:
        return {"ok": False}
    with open(marker, "w") as f:
        f.write(stamp + "\n")
    return {"ok": True}


# ---- monitors


SADC = next((p for p in ("/usr/lib/sysstat/sadc", "/usr/libexec/sysstat/sadc") if os.path.exists(p)), "sadc")


def monitor_start(outdir, phase, pidfile=""):
    """Record system activity each second (sadc), and the server process's (pidstat) if PIDFILE
    names one, until monitor_stop."""
    os.makedirs(outdir, exist_ok=True)
    procs = {"sadc": subprocess.Popen([SADC, "-S", "XDISK", "1", os.path.join(outdir, f"{phase}.sa")],
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, start_new_session=True).pid}
    monitors = [("users", ["-u", "-U", getpass.getuser()])]  # every process of this user: servers fork
    if pidfile and os.path.exists(pidfile):
        monitors.append(("process", ["-u", "-r", "-d", "-p", open(pidfile).read().strip()]))
    for label, args in monitors:
        with open(os.path.join(outdir, f"{phase}.{label}.pidstat"), "w") as out:
            procs[label] = subprocess.Popen(["pidstat", "-h", *args, "1"], stdin=subprocess.DEVNULL, stdout=out,
                                            stderr=subprocess.STDOUT, start_new_session=True,
                                            env=dict(os.environ, LC_ALL="C")).pid
    util.write_json(os.path.join(outdir, f"{phase}.monitor.json"), procs)
    return procs


def monitor_stop(outdir, phase, data):
    """Stop the monitors monitor_start started, write CSV conversions beside the recordings, and
    return a summary: CPU, steal, the data disk, the network card, dirty memory, and the server
    process."""
    procs = util.read_json(os.path.join(outdir, f"{phase}.monitor.json"))
    for label, pid in procs.items():
        _kill(pid, "sadc" if label == "sadc" else "pidstat")
    sa = os.path.join(outdir, f"{phase}.sa")
    dev, disk = conf()["DEV"], disk_name(data)
    cpu = _sadf(sa, ["-u"], os.path.join(outdir, f"{phase}.cpu.csv"))
    dsk = _sadf(sa, ["-d"], os.path.join(outdir, f"{phase}.disk.csv"))
    net = _sadf(sa, ["-n", "DEV"], os.path.join(outdir, f"{phase}.net.csv"))
    mem = _sadf(sa, ["-r", "ALL"], os.path.join(outdir, f"{phase}.mem.csv"))
    ncpu = os.cpu_count()
    cpu = [r for r in cpu if r.get("CPU") in ("-1", "all")]
    dsk = [r for r in dsk if r.get("DEV") == disk]
    net = [r for r in net if r.get("IFACE") == dev]
    out = {
        "seconds": len(cpu),
        "cpu_busy_cores": _mean([(100 - float(r["%idle"])) * ncpu / 100 for r in cpu]),
        "cpu_steal_pct": _mean([float(r.get("%steal", 0)) for r in cpu]),
        # User-space time only: unlike busy time it leaves out I/O wait and the kernel's own work
        # for the processes being measured, so it is what other load would show up in.
        "cpu_user_cores": _mean([(float(r["%user"]) + float(r["%nice"])) * ncpu / 100 for r in cpu]),
        "disk": {"device": disk,
                 "util_avg": _mean([float(r["%util"]) for r in dsk]),
                 "util_max": max([float(r["%util"]) for r in dsk], default=0.0),
                 "read_bytes": sum(float(r["rkB/s"]) for r in dsk) * 1024,
                 "written_bytes": sum(float(r["wkB/s"]) for r in dsk) * 1024},
        "net": {"rx_mbit_max": max([float(r["rxkB/s"]) * 8 / 1000 for r in net], default=0.0),
                "tx_mbit_max": max([float(r["txkB/s"]) * 8 / 1000 for r in net], default=0.0)},
        "dirty_peak_bytes": max([int(r["kbdirty"]) * 1024 for r in mem], default=0),
        "mem_used_rise_bytes": (max(int(r["kbmemused"]) for r in mem) - int(mem[0]["kbmemused"])) * 1024 if mem else 0,
    }
    for label in ("users", "process"):
        path = os.path.join(outdir, f"{phase}.{label}.pidstat")
        if os.path.exists(path):
            out[label] = _pidstat(path)
    return out


# ---- servers


def p4_reset(cfg):
    """A fresh P4 server: empty root, the configurables and typemap CFG names, one super user
    with a password (security=4 needs one). CFG: dir, p4d, p4, port (tcp or ssl), password,
    configurables, typemap (spec text or null)."""
    d, p4d, p4 = cfg["dir"], cfg["p4d"], cfg["p4"]
    pidfile = os.path.join(d, "p4d.pid")
    _kill_pidfile(pidfile, exe=p4d)
    for sub in ("root", "logs", "ssl"):
        shutil.rmtree(os.path.join(d, sub), ignore_errors=True)
    os.makedirs(os.path.join(d, "root"))
    os.makedirs(os.path.join(d, "logs"))
    env = dict(os.environ, P4USER="bench", P4TICKETS=os.path.join(d, "p4tickets"), P4ENVIRO="/dev/null",
               P4CONFIG="", P4TRUST=os.path.join(d, "p4trust"))
    port, local = "1666", "localhost:1666"
    if cfg["port"] == "ssl":
        ssl = os.path.join(d, "ssl")
        os.makedirs(ssl, mode=0o700)
        env["P4SSLDIR"] = ssl
        sh([p4d, "-Gc"], env=env)
        port, local = "ssl:1666", "ssl:localhost:1666"
    env["P4PORT"] = local
    # p4d -d forks a daemon that keeps its parent's standard output and error open, so they must
    # go to a file: reading them through a pipe would wait for the daemon to exit.
    with open(os.path.join(d, "logs", "p4d.out"), "w") as out:
        subprocess.run([p4d, "-r", os.path.join(d, "root"), "-p", port, "-L", os.path.join(d, "logs", "log"),
                        "-J", os.path.join(d, "root", "journal"), f"--pid-file={pidfile}", "-d"],
                       env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT, check=True)
    for _ in range(100):
        if cfg["port"] == "ssl":
            subprocess.run([p4, "trust", "-y", "-f"], env=env, capture_output=True)
        if subprocess.run([p4, "-s", "info"], env=env, capture_output=True).returncode == 0:
            break
        time.sleep(0.2)
    pw = cfg["password"]
    sh([p4, "passwd"], input=f"{pw}\n{pw}\n", env=env)
    sh([p4, "login"], input=f"{pw}\n", env=env)
    sh([p4, "user", "-i", "-f"], input=sh([p4, "user", "-o"], env=env), env=env)
    for key, value in cfg["configurables"].items():
        sh([p4, "configure", "set", f"{key}={value}"], env=env)
    if cfg.get("typemap"):
        sh([p4, "typemap", "-i"], input=cfg["typemap"], env=env)
    sh([p4, "configure", "set", "security=4"], env=env)
    return {"version": _first(r"^(Rev\..*)$", sh([p4d, "-V"])), "pidfile": pidfile,
            "configure": sh([p4, "configure", "show"], env=env).strip().splitlines(),
            "typemap": sh([p4, "typemap", "-o"], env=env).split("TypeMap:")[-1].strip().splitlines()}


def lore_reset(cfg):
    """A fresh Lore server: empty store, the compiled-in defaults plus CFG's server_config, and a
    self-signed QUIC certificate (lore:// clients don't verify it by default). CFG: dir,
    loreserver, server_config (TOML text or null)."""
    d, binary = cfg["dir"], cfg["loreserver"]
    pidfile = os.path.join(d, "loreserver.pid")
    _kill_pidfile(pidfile, exe=binary)
    shutil.rmtree(os.path.join(d, "store"), ignore_errors=True)
    for sub in ("store", "logs", "config"):
        os.makedirs(os.path.join(d, sub), exist_ok=True)
    config = os.path.join(d, "config")
    cert, key = os.path.join(config, "cert.pem"), os.path.join(config, "key.pem")
    if not os.path.exists(cert):
        sh(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
            "-keyout", key, "-out", cert, "-days", "3650", "-subj", "/CN=bench-lore-server"])
    store = os.path.join(d, "store")
    with open(os.path.join(config, "local.toml"), "w") as f:
        f.write(f'[server.quic.certificate]\ncert_file = "{cert}"\npkey_file = "{key}"\n\n'
                f'[immutable_store.local]\npath = "{store}"\n\n[mutable_store.local]\npath = "{store}"\n')
    default = os.path.join(config, "default.toml")
    if cfg.get("server_config"):
        with open(default, "w") as f:
            f.write(cfg["server_config"])
    elif os.path.exists(default):
        os.remove(default)
    log = open(os.path.join(d, "logs", f"server-{time.strftime('%Y%m%d-%H%M%S')}.log"), "w")
    proc = subprocess.Popen([binary, "--config", config], cwd=d, stdin=subprocess.DEVNULL, stdout=log,
                            stderr=subprocess.STDOUT, start_new_session=True)
    with open(pidfile, "w") as f:
        f.write(f"{proc.pid}\n")
    code = None
    deadline = time.monotonic() + 240  # well inside the lore-reset operation's 300 s
    while time.monotonic() < deadline:
        try:
            code = urllib.request.urlopen("http://127.0.0.1:41339/health_check", timeout=1).status
            break
        except OSError:
            if proc.poll() is not None:
                raise RuntimeError(f"loreserver exited {proc.returncode}; see {log.name}")
            time.sleep(0.2)
    if code != 200:
        raise RuntimeError(f"loreserver did not answer its health check; see {log.name}")
    return {"version": sh([binary, "--version"]).strip(), "pidfile": pidfile, "log": log.name}


def stop_servers(base):
    """Stop any server a reset started under BASE (base/p4, base/lore)."""
    _kill_pidfile(os.path.join(base, "p4", "p4d.pid"), "p4d")
    _kill_pidfile(os.path.join(base, "lore", "loreserver.pid"), "loreserver")
    return {}


def latest_log(directory, pattern):
    names = sorted((n for n in os.listdir(directory) if re.fullmatch(pattern, n)),
                   key=lambda n: os.stat(os.path.join(directory, n)).st_mtime)
    return {"path": os.path.join(directory, names[-1]) if names else ""}


# ---- helpers


def _read(path):
    try:
        return open(path).read()
    except OSError:
        return ""


def _sha256(path):
    try:
        return util.sha256_file(path)
    except OSError:
        return ""


def _first(pattern, text):
    m = re.search(pattern, text, re.M)
    return m.group(1).strip() if m else ""


def _version(argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True).stdout.splitlines()[0].strip()
    except (OSError, IndexError):
        return ""


def _snmp():
    return parse_snmp(_read("/proc/net/snmp"))


def parse_snmp(text):
    """/proc/net/snmp as {(protocol, counter): value}: a header line of names, then a line of values."""
    lines = text.splitlines()
    out = {}
    for header, values in zip(lines[::2], lines[1::2]):
        proto, names = header.split(":")
        for name, value in zip(names.split(), values.split(":")[1].split()):
            out[(proto, name)] = int(value)
    return out


def _mean(values):
    return sum(values) / len(values) if values else 0.0


def _sadf(sa, report, csv_path):
    out = subprocess.run(["sadf", "-d", sa, "--", *report], capture_output=True, text=True,
                         env=dict(os.environ, LC_ALL="C")).stdout
    with open(csv_path, "w") as f:
        f.write(out)
    rows, header = [], None
    for line in out.splitlines():
        if line.startswith("# "):
            header = line[2:].split(";")
        elif header and line:
            rows.append(dict(zip(header, line.split(";"))))
    return rows


def _pidstat(path):
    cpu_s, usr_s, rss, header = 0.0, 0.0, 0, None
    for line in open(path):
        if line.startswith("#"):
            header = line[1:].split()
        elif header and line.strip():
            fields = dict(zip(header, line.split()))
            try:
                cpu_s += float(fields.get("%CPU", 0)) / 100
                usr_s += float(fields.get("%usr", 0)) / 100
                rss = max(rss, int(fields.get("RSS", 0)) * 1024)
            except ValueError:
                pass
    return {"cpu_s": cpu_s, "usr_s": usr_s, "peak_rss_bytes": rss}


def _kill(pid, name):
    """Stop process PID if it is still the NAME this harness started (checked through /proc, in
    case the PID was reused), and wait for it to go."""
    try:
        cmdline = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")[0].decode()
    except OSError:
        return
    if os.path.basename(cmdline) != name:
        return
    os.kill(pid, signal.SIGTERM)
    for _ in range(100):
        if not os.path.exists(f"/proc/{pid}"):
            return
        time.sleep(0.1)
    os.kill(pid, signal.SIGKILL)


def _kill_pidfile(pidfile, name=None, exe=None):
    try:
        pid = int(open(pidfile).read().strip())
    except (OSError, ValueError):
        return
    try:
        actual = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        return
    if exe and os.path.realpath(actual) != os.path.realpath(exe):
        return
    if name and os.path.basename(actual) != name:
        return
    os.kill(pid, signal.SIGTERM)
    for _ in range(300):
        if not os.path.exists(f"/proc/{pid}"):
            return
        time.sleep(0.1)
    os.kill(pid, signal.SIGKILL)


OPS = {
    "fingerprint": lambda data: fingerprint(data),
    "sysctl": lambda: apply_sysctl(json.load(sys.stdin)),
    "shape": shape, "unshape": unshape, "wire": wire,
    "iperf-start": iperf_start, "iperf-stop": iperf_stop,
    "trim": trim, "dropcaches": dropcaches, "flush": flush, "fio": fio, "du": du, "have": have,
    "monitor-start": monitor_start, "monitor-stop": monitor_stop,
    "p4-reset": lambda: p4_reset(json.load(sys.stdin)),
    "lore-reset": lambda: lore_reset(json.load(sys.stdin)),
    "stop-servers": stop_servers, "latest-log": latest_log,
}


def main(argv):
    if not argv or argv[0] not in OPS:
        print(f"usage: bench hostops {{{','.join(OPS)}}} ARGS...", file=sys.stderr)
        return 2
    json.dump(OPS[argv[0]](*argv[1:]), sys.stdout)
    print()
    return 0
