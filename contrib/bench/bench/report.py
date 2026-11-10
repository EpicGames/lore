"""Reports from collected run directories.

    bench report RUN_DIR [--out FILE]              the matrix report of one run
    bench report RUN_DIR RUN_DIR... --trend [--out FILE]
                                                   one table and chart per series over tool builds

The copy baseline in the matrix report is computed from rated speeds, not measured: the corpus's
uncompressed bytes through the slowest of the sender's disk read, the link, and the receiver's disk
write, with the receiver's page cache taking the first vm.dirty_ratio of its memory without the disk.
"""
import glob
import html
import os
import statistics

from . import util
from .agent import verified

COLORS = {"p4": "#5b8def", "lore": "#e8a33d"}
# A phase with one of these flags has no meaningful time: it is listed, but left out of the charts
# and the medians.
FAILED = {"step-failed", "stalled", "verify-failed"}
NAMES = {"p4": "P4", "lore": "Lore"}
CSS = """
body{font:15px/1.5 system-ui,sans-serif;margin:2em auto;max-width:1100px;padding:0 1em;color:#1d2330}
h1{font-size:30px;margin-bottom:.2em} h2{font-size:24px;margin-top:2em;border-bottom:1px solid #ccd}
h3{font-size:18px;margin-top:1.5em} .meta{color:#556}
table{border-collapse:collapse;margin:.8em 0} td,th{border:1px solid #d5d9e2;padding:4px 10px;text-align:left}
td.num,th.num{text-align:right;font-variant-numeric:tabular-nums} th{background:#f1f3f8}
.flag{color:#b3261e;font-weight:600} .ok{color:#1e7b34} svg text{font:12px system-ui,sans-serif}
"""


def load_runs(run_dir):
    results = []
    for path in sorted(glob.glob(os.path.join(run_dir, "*", "*", "result.json"))):
        r = util.read_json(path)
        r["_dir"] = os.path.dirname(path)
        results.append(r)
    return results


def baseline(phase, prof, corpus_bytes, inv, fps):
    """(seconds, what sets it) for copying the uncompressed corpus at rated speeds."""
    sender, receiver = ("client", "server") if phase == "import" else ("server", "client")
    terms = {}
    if prof.get("rate_mbit"):
        terms["the link"] = corpus_bytes * 8 / (prof["rate_mbit"] * 1e6)
    rs, rr = inv[sender].get("rated", {}), inv[receiver].get("rated", {})
    if rs.get("read_mbs"):
        terms[f"reading on the {sender}"] = corpus_bytes / (rs["read_mbs"] * 1e6)
    if rr.get("write_mbs"):
        fp = fps[receiver]
        dirty_bytes = int(fp["sysctl"].get("vm.dirty_bytes", "0") or 0)
        absorbed = dirty_bytes or int(fp["sysctl"].get("vm.dirty_ratio", "20")) / 100 * fp["mem_bytes"]
        to_disk = max(0.0, corpus_bytes - absorbed)
        if rr.get("cache_gb") and rr.get("after_cache_mbs"):
            fast = min(to_disk, rr["cache_gb"] * 1e9)
            t = fast / (rr["write_mbs"] * 1e6) + (to_disk - fast) / (rr["after_cache_mbs"] * 1e6)
        else:
            t = to_disk / (rr["write_mbs"] * 1e6)
        terms[f"writing on the {receiver}"] = t
    if not terms:
        return None, None
    lim = max(terms, key=terms.get)
    return terms[lim], lim


def usable(phases):
    """The phases whose time means something: none failed, stalled or failed its verification."""
    return [p for p in phases if not FAILED & set(p["flags"])]


def tick(span, ticks):
    """A round step that puts at most TICKS ticks on an axis of SPAN seconds."""
    for s in (30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 14400, 21600, 43200):
        if span / s <= ticks:
            return s
    return 86400 * -(-span // (86400 * ticks))


def bars(rows, line=None, line_label=""):
    """A horizontal bar chart: ROWS of (label, seconds, color), and an optional dashed line."""
    top = max([v for _, v, _ in rows] + [line or 0]) * 1.12 or 1
    step = tick(top, 8)
    w, left, h, bar = 900, 170, 34, 22
    height = len(rows) * h + 40
    x = lambda v: left + (w - left - 20) * v / top
    out = [f'<svg width="{w}" height="{height}" role="img">']
    t = 0
    while t <= top:
        out.append(f'<line x1="{x(t):.1f}" y1="10" x2="{x(t):.1f}" y2="{height - 25}" stroke="#e3e6ee"/>'
                   f'<text x="{x(t):.1f}" y="{height - 8}" text-anchor="middle">{util.mmss(t).split(".")[0]}</text>')
        t += step
    for i, (label, v, color) in enumerate(rows):
        y = 12 + i * h
        out.append(f'<text x="{left - 8}" y="{y + bar * .7:.1f}" text-anchor="end">{html.escape(label)}</text>'
                   f'<rect x="{left}" y="{y}" width="{x(v) - left:.1f}" height="{bar}" fill="{color}"/>'
                   f'<text x="{x(v) + 6:.1f}" y="{y + bar * .7:.1f}">{util.mmss(v)}</text>')
    if line:
        out.append(f'<line x1="{x(line):.1f}" y1="6" x2="{x(line):.1f}" y2="{height - 25}" stroke="#1d2330" '
                   f'stroke-width="2" stroke-dasharray="5 4"/><text x="{x(line) + 4:.1f}" y="10">'
                   f'{html.escape(line_label)}</text>')
    out.append("</svg>")
    return "".join(out)


def timeline(series, ymax, unit="Mbit/s"):
    """A line chart of per-second values: SERIES of (label, color, values)."""
    w, h, left, bottom = 900, 220, 60, 30
    xmax = max([len(v) for _, _, v in series] + [1])
    ymax = ymax or max([max(v or [0]) for _, _, v in series] + [1])
    x = lambda i: left + (w - left - 20) * i / xmax
    y = lambda v: h - bottom - (h - bottom - 10) * min(v, ymax) / ymax
    out = [f'<svg width="{w}" height="{h}" role="img">']
    for frac in (0, .25, .5, .75, 1):
        out.append(f'<line x1="{left}" y1="{y(ymax * frac):.1f}" x2="{w - 20}" y2="{y(ymax * frac):.1f}" stroke="#e3e6ee"/>'
                   f'<text x="{left - 6}" y="{y(ymax * frac) + 4:.1f}" text-anchor="end">{ymax * frac:,.0f}</text>')
    step = tick(xmax, 10)
    for t in range(0, xmax + 1, step):
        out.append(f'<text x="{x(t):.1f}" y="{h - 10}" text-anchor="middle">{util.mmss(t).split(".")[0]}</text>')
    for label, color, values in series:
        pts = " ".join(f"{x(i):.1f},{y(v):.1f}" for i, v in enumerate(values))
        out.append(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="1.5"/>')
    legend = " ".join(f'<span style="color:{c}">&#9632;</span> {html.escape(l)}' for l, c, _ in series)
    out.append("</svg>")
    return f"<div>{legend} <span class='meta'>({unit} per second)</span></div>" + "".join(out)


def net_series(csv_path, iface, column):
    """Per-second Mbit/s on IFACE from a sadf -n DEV conversion."""
    if not os.path.exists(csv_path):
        return []
    header, out = None, []
    for line in open(csv_path):
        if line.startswith("# "):
            header = line[2:].strip().split(";")
        elif header and line.strip():
            row = dict(zip(header, line.strip().split(";")))
            if row.get("IFACE") == iface:
                out.append(float(row[column]) * 8 / 1000)
    return out


def median_range(values, fmt):
    values = sorted(v for v in values if v is not None)
    if not values:
        return "–"
    med = statistics.median(values)
    return fmt(med) if len(values) == 1 else f"{fmt(med)} ({fmt(values[0])}–{fmt(values[-1])})"


def gb(n):
    return f"{n / 1e9:.2f} GB"


def mib(n):
    return f"{n / 2**20:,.0f} MiB"


def matrix(run_dir):
    spec = util.read_json(os.path.join(run_dir, "spec.json"))
    prep = util.read_json(os.path.join(run_dir, "prepare", "prepared.json"))
    inv, fps, info = spec["_inventory"], prep["fingerprint"], prep["corpus"]
    results = load_runs(run_dir)
    tools = list(spec["tool"])
    e = html.escape
    harnesses = sorted({(r.get("harness_sha256") or "")[:12] for r in results}) or [(spec.get("_harness_sha256") or "")[:12]]
    out = [f"<!doctype html><meta charset=utf-8><title>{e(spec['name'])}</title><style>{CSS}</style>",
           f"<h1>{e(spec['name'])}</h1>",
           f"<p class=meta>Run {e(os.path.basename(run_dir))}. Spec {spec['_sha256'][:12]}, "
           f"{'harness' if len(harnesses) == 1 else 'harnesses'} {', '.join(harnesses)}. Corpus {info['id'][:12]}: "
           f"{info['files']:,} files, {gb(info['bytes'])} uncompressed.</p>"]
    versions = {}
    for r in results:
        versions[r["tool"]["name"]] = r["tool"]["version"]
    out.append("<p>" + "; ".join(f"{NAMES.get(t, t)} {e(v)}" for t, v in versions.items()) + "</p>")
    out.append("<h2>Hosts</h2><table><tr><th></th><th>Client</th><th>Server</th></tr>")
    rows = [("Notes", lambda r: inv[r].get("notes", "")),
            ("CPU", lambda r: f"{fps[r]['cpus']} × {fps[r]['cpu_model']}"),
            ("Memory", lambda r: f"{fps[r]['mem_bytes'] / 2**30:.0f} GiB"),
            ("OS and kernel", lambda r: f"{fps[r]['os']}, {fps[r]['kernel']} ({fps[r]['virt']})"),
            ("Data disk", lambda r: f"{fps[r]['disk']['fstype']} on {fps[r]['disk']['device']} {fps[r]['disk']['model']}"),
            ("Rated disk", lambda r: ", ".join(f"{k} {v}" for k, v in inv[r].get("rated", {}).items())),
            ("Network card", lambda r: f"{fps[r]['nic']['driver']}, MTU {fps[r]['nic']['mtu']}"),
            ("Dirty ratio", lambda r: f"{fps[r]['sysctl']['vm.dirty_ratio']}% of memory"),
            ("TCP buffers (rmem, wmem)", lambda r: f"{fps[r]['sysctl']['net.ipv4.tcp_rmem']}; {fps[r]['sysctl']['net.ipv4.tcp_wmem']}")]
    for label, get in rows:
        out.append(f"<tr><th>{label}</th><td>{e(get('client'))}</td><td>{e(get('server'))}</td></tr>")
    out.append("</table>")
    for prof in spec["profile"]:
        pr = [r for r in results if r["profile"]["name"] == prof["name"]]
        rate = f", {prof['rate_mbit']:,} Mbit/s" if prof.get("rate_mbit") else ", no rate limit"
        out.append(f"<h2>{e(prof['name'])}: {prof['rtt_ms']} ms round trip{rate}</h2>")
        cal_path = os.path.join(run_dir, prof["name"], "calibration.json")
        if os.path.exists(cal_path):
            out.append(calibration_table(util.read_json(cal_path), prep.get("base_rtt_ms", {})))
        for test in spec["run"]["tests"]:
            out.append(test_section(test, prof, pr, tools, info, inv, fps))
    return "".join(out)


def calibration_table(cal, base):
    """The calibration CAL of one profile. BASE is the unshaped round trip measured when the run was
    prepared, for runs whose calibration doesn't record the one measured for the profile."""
    e = html.escape
    tcp = cal.get("tcp", {})
    base = cal.get("base_rtt_ms", base)
    added = f", {cal['delay_us_each_way'] / 1000:.2f} ms added each way" if "delay_us_each_way" in cal else ""
    rows = [("Idle round trip", f"{cal['rtt_idle_ms']['median']:.1f} ms (unshaped {base.get('median', 0):.2f} ms{added})"),
            ("Round trip under a 4-stream download",
             f"{cal['rtt_loaded_ms'].get('median', 0):.1f} ms, max {cal['rtt_loaded_ms'].get('max', 0):.1f} ms"),
            ("TCP, client to server (1 / 4 streams)",
             f"{tcp.get('up_1', {}).get('mbit', 0):,.0f} / {tcp.get('up_4', {}).get('mbit', 0):,.0f} Mbit/s"),
            ("TCP, server to client (1 / 4 streams)",
             f"{tcp.get('down_1', {}).get('mbit', 0):,.0f} / {tcp.get('down_4', {}).get('mbit', 0):,.0f} Mbit/s")]
    for k, v in cal.get("udp", {}).items():
        rows.append((f"UDP {'client to server' if k == 'up' else 'server to client'}",
                     f"{v.get('mbit', 0):,.0f} Mbit/s, {v.get('lost_pct') or 0:.2f}% lost"))
    for role, d in cal.get("disk", {}).items():
        rows.append((f"{role.title()} disk (sequential write / read, small files)",
                     f"{d['seq_write_mbs']:,.0f} / {d['seq_read_mbs']:,.0f}, {d['small_write_mbs']:,.0f} MB/s"))
    status = "<span class=ok>passed</span>" if not cal.get("problems") else \
        "<span class=flag>" + e("; ".join(cal["problems"])) + "</span>"
    rows.append(("Calibration gate", status))
    return "<h3>Calibration</h3><table>" + "".join(
        f"<tr><th>{k}</th><td>{v if k == 'Calibration gate' else e(v)}</td></tr>" for k, v in rows) + "</table>"


def test_section(test, prof, results, tools, info, inv, fps):
    e = html.escape
    title = "Import" if test == "import" else "Fetch"
    what = ("P4 add and submit; Lore stage, commit and push" if test == "import"
            else "P4 sync into a new workspace; Lore clone")
    every = {t: [r["phases"][test] for r in results if r["tool"]["name"] == t and test in r["phases"]] for t in tools}
    by_tool = {t: usable(ps) for t, ps in every.items()}
    out = [f"<h3>{title}</h3><p class=meta>{what}. Shorter is better. A run that failed, stalled or "
           f"didn't verify is left out of the chart and the figures, and listed under the flags.</p>"]
    line, lim = baseline(test, prof, info["bytes"], inv, fps)
    rows = [(NAMES.get(t, t), statistics.median(p["wall_s"] for p in ps), COLORS.get(t, "#888"))
            for t, ps in by_tool.items() if ps]
    if rows:
        out.append(bars(rows, line, f"baseline {util.mmss(line)}" if line else ""))
        if line:
            out.append(f"<p class=meta>The dashed line is a baseline: copying the uncompressed "
                       f"{gb(info['bytes'])} at rated speeds takes {util.mmss(line)}, set by {e(lim)}.</p>")
    metrics = [
        ("Wall clock", lambda p: p["wall_s"], util.mmss),
        ("Bytes client → server", lambda p: p["wire_bytes"]["client_to_server"], gb),
        ("Bytes server → client", lambda p: p["wire_bytes"]["server_to_client"], gb),
        ("Client CPU", lambda p: p["client_cpu_s"], lambda v: f"{v:,.0f} s"),
        ("Client peak memory", lambda p: p["client_peak_rss_bytes"], mib),
        ("Server CPU (the benchmark user's processes)", lambda p: p["monitor"]["server"].get("users", {}).get("cpu_s"),
         lambda v: f"{v:,.0f} s"),
        ("Server main process peak memory", lambda p: p["monitor"]["server"].get("process", {}).get("peak_rss_bytes"), mib),
        ("Server memory in use, peak rise", lambda p: p["monitor"]["server"]["mem_used_rise_bytes"], mib),
        ("Client disk written", lambda p: p["monitor"]["client"]["disk"]["written_bytes"], gb),
        ("Server disk written", lambda p: p["monitor"]["server"]["disk"]["written_bytes"], gb),
        ("Client disk busy (avg)", lambda p: p["monitor"]["client"]["disk"]["util_avg"], lambda v: f"{v:.0f}%"),
        ("Server disk busy (avg)", lambda p: p["monitor"]["server"]["disk"]["util_avg"], lambda v: f"{v:.0f}%"),
        ("Client peak unwritten data", lambda p: p["monitor"]["client"]["dirty_peak_bytes"], gb),
        ("Flush afterwards, client (not in wall clock)", lambda p: p["flush_s"]["client"], util.mmss),
        ("Flush afterwards, server (not in wall clock)", lambda p: p["flush_s"]["server"], util.mmss),
        ("TCP retransmits (client / server)", "tcp_retrans_segs", None),
        ("UDP receive-buffer drops (client / server)", "udp_rcvbuf_errors", None),
    ]
    out.append("<table><tr><th></th>" + "".join(f"<th class=num>{NAMES.get(t, t)}</th>" for t in by_tool) + "</tr>")
    for label, get, fmt in metrics:
        if fmt is None:  # a wire counter on both hosts, every repetition; "–" where it wasn't recorded
            count = lambda v: "–" if v is None else f"{v:,}"
            cells = [", ".join(f"{count(p['wire']['client'].get(get))} / {count(p['wire']['server'].get(get))}" for p in ps)
                     for ps in by_tool.values()]
        else:
            cells = [median_range([get(p) for p in ps], fmt) for ps in by_tool.values()]
        out.append(f"<tr><th>{label}</th>" + "".join(f"<td class=num>{e(c)}</td>" for c in cells) + "</tr>")
    steps = ["; ".join(f"{s['name']} {util.mmss(s['wall_s'])}" for s in ps[0]["steps"]) if ps else "" for ps in by_tool.values()]
    out.append("<tr><th>Steps (first usable repetition)</th>" + "".join(f"<td class=num>{e(s)}</td>" for s in steps) + "</tr>")
    checks = []
    for ps in every.values():
        notes = sorted({f for p in ps for f in p["flags"]})
        v = [p["verify"] for p in ps if "verify" in p]
        if v:
            ok = all(verified(x) for x in v)
            notes.append(f"{v[0]['fetched_files']:,} files {'verified' if ok else 'DIFFER'}"
                         + (" (SHA-256)" if any("sha256_match" in x for x in v) else ""))
        checks.append(", ".join(notes) or "none")
    out.append("<tr><th>Flags and verification</th>" + "".join(
        f"<td class=num>{e(c)}</td>" for c in checks) + "</tr></table>")
    column = "txkB/s" if test == "import" else "rxkB/s"
    series = []
    for t in tools:
        rs = [r for r in results if r["tool"]["name"] == t and test in r["phases"] and usable([r["phases"][test]])]
        if rs:
            iface = fps["client"]["nic"]["dev"]
            fp_path = rs[0]["_dir"]
            series.append((NAMES.get(t, t), COLORS.get(t, "#888"),
                           net_series(os.path.join(fp_path, f"{test}.net.csv"), iface, column)))
    if any(s[2] for s in series):
        out.append(f"<p class=meta>Client network, {'sent' if test == 'import' else 'received'}, first usable "
                   "repetition:</p>")
        out.append(timeline(series, prof.get("rate_mbit")))
    return "".join(out)


def trend(run_dirs):
    """One table and chart per series: every result's phase, grouped by series key, over tool
    builds (by their binaries' SHA-256, as unstamped builds share a version) in the order they were
    first run. Flagged phases are counted but left out of the medians; a build with no unflagged
    phase has none."""
    points = {}
    for d in run_dirs:
        for r in load_runs(d):
            for test, p in r["phases"].items():
                points.setdefault(p["series"], []).append((r, test, p))
    e = html.escape
    out = [f"<!doctype html><meta charset=utf-8><title>Trends</title><style>{CSS}</style><h1>Trends</h1>"]
    for key, items in points.items():
        r0, test, _ = items[0]
        out.append(f"<h2>{NAMES.get(r0['tool']['name'], r0['tool']['name'])} {test}, "
                   f"{e(r0['profile']['name'])}</h2><p class=meta>Series {key[:12]}</p>")
        by_build = {}
        for r, _, p in sorted(items, key=lambda x: x[0]["started"]):
            by_build.setdefault(util.digest(r["tool"]["binaries"]), []).append((r, p))
        table = ["<table><tr><th>Build</th><th>First run</th><th class=num>Runs</th><th class=num>Wall clock</th>"
                 "<th class=num>Bytes on the wire</th><th class=num>Client CPU</th><th class=num>Client peak memory</th>"
                 "<th>Flags</th></tr>"]
        medians, chart = [], []
        color = COLORS.get(r0["tool"]["name"], "#888")
        for build, rs in by_build.items():
            label = f"{rs[0][0]['tool']['version']} ({build[:8]})"
            clean = [p for _, p in rs if not p["flags"]]
            outside = False
            if clean:
                med = statistics.median(p["wall_s"] for p in clean)
                prev = medians[-5:]
                outside = prev and not (min(prev) <= med <= max(prev))
                medians.append(med)
                chart.append((label, med, color))
            flags = sorted({f for _, p in rs for f in p["flags"]})
            table.append(
                f"<tr><td>{e(label)}</td><td>{rs[0][0]['started']}</td><td class=num>{len(rs)}</td>"
                f"<td class=num>{'<b>' if outside else ''}{median_range([p['wall_s'] for p in clean], util.mmss)}"
                f"{'</b>' if outside else ''}</td>"
                f"<td class=num>{median_range([sum(p['wire_bytes'].values()) for p in clean], gb)}</td>"
                f"<td class=num>{median_range([p['client_cpu_s'] for p in clean], lambda v: f'{v:,.0f} s')}</td>"
                f"<td class=num>{median_range([p['client_peak_rss_bytes'] for p in clean], mib)}</td>"
                f"<td class=flag>{e(', '.join(flags))}</td></tr>")
        if chart:
            out.append(bars(chart))
        out.append("".join(table) + "</table><p class=meta>Medians are of the runs with no flags. A median in "
                   "bold falls outside the range of the five builds before it.</p>")
    return "".join(out)
