"""Load, check and hash a benchmark spec: a TOML file naming the corpus, the target, the host
settings, the network profiles, the tools and the run options. examples/ has complete specs.

A spec's identity is the hash of its parsed content together with the content of every file it
refers to, so reformatting a spec leaves the hash unchanged and editing a referenced file changes it.
"""
import os
import re
import tomllib

from . import METHOD, inventory, util

HEX64 = re.compile(r"[0-9a-f]{64}")
NAME = re.compile(r"[a-z0-9][a-z0-9-]{0,62}")
URL_SCHEMES = ("http://", "https://", "file://", "s3://", "gs://", "gdrive://")
TESTS = ("import", "fetch")
SYSCTL_KEYS = (
    "net.ipv4.tcp_rmem", "net.ipv4.tcp_wmem", "net.core.rmem_max", "net.core.wmem_max",
    "net.core.rmem_default", "net.core.wmem_default", "net.core.netdev_max_backlog",
    "net.ipv4.tcp_congestion_control", "net.core.default_qdisc",
    "vm.dirty_ratio", "vm.dirty_background_ratio", "vm.dirty_bytes", "vm.dirty_background_bytes",
    "vm.swappiness",
)
# Tool settings that name binaries. They are left out of the series key: the binary is the version,
# which is what a trend plots.
BINARIES = {"p4": ("p4d", "p4"), "lore": ("lore", "loreserver")}
PROFILE_KEYS = ("rtt_ms", "rate_mbit", "queue_bdp", "loss_ppm", "jitter_ms")
MAX_QUEUE_BYTES = 1 << 30  # the largest shaper queue host/bench-helper accepts


def queue_bytes(profile):
    """The shaper's queue for PROFILE: queue_bdp bandwidth-delay products, in bytes."""
    rate = profile.get("rate_mbit") or 0
    return int(profile.get("queue_bdp", 1.0) * rate * 1e6 / 8 * profile.get("rtt_ms", 0) / 1000) if rate else 0


class SpecError(Exception):
    def __init__(self, errors):
        super().__init__("\n".join(errors))
        self.errors = errors


def load(path):
    """Parse and check the spec at PATH. Returns it with defaults filled in, the files it refers to
    under "_files" (each with its text and hash), the parsed inventory under "_inventory", and its
    identity under "_sha256". Raises SpecError listing every problem found."""
    try:
        with open(path, "rb") as f:
            raw = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise SpecError([f"{path}: {e}"]) from None
    checker = _Checker(os.path.dirname(os.path.abspath(path)))
    spec = checker.spec(raw)
    if checker.errors:
        raise SpecError(checker.errors)
    spec["_files"] = checker.files
    if "inventory" in checker.files:
        try:
            spec["_inventory"] = inventory.parse(checker.files["inventory"]["text"], checker.files["inventory"]["path"])
        except inventory.InventoryError as e:
            raise SpecError([f"target.inventory: {m}" for m in e.errors]) from None
    spec["_sha256"] = identity(spec)
    return spec


def identity(spec):
    body = {k: v for k, v in spec.items() if not k.startswith("_")}
    body["files"] = {name: f["sha256"] for name, f in sorted(spec.get("_files", {}).items())}
    return util.digest(body)


def tool_settings(spec, tool):
    """TOOL's settings without its binaries, with referenced files replaced by their hashes."""
    settings = {k: v for k, v in spec["tool"][tool].items() if k not in BINARIES[tool]}
    for key in ("typemap", "server_config"):
        name = f"{tool}.{key}"
        if name in spec.get("_files", {}):
            settings[key] = {"sha256": spec["_files"][name]["sha256"]}
    return settings


def series_key(spec, profile, test, tool, corpus_id, infra_class):
    """What makes two results comparable: everything that affects the numbers except the tool's
    version, which is a trend's x-axis."""
    return util.digest({
        "method": METHOD,
        "corpus": corpus_id,
        "profile": {k: profile.get(k) for k in PROFILE_KEYS},
        "test": test,
        "tool": tool,
        "settings": tool_settings(spec, tool),
        "host": spec["host"],
        "infra": infra_class,
    })


def matrix(spec):
    """(profile, repetition, position, tool) for every run, in the order they run. With order
    "alternate", even repetitions run the tools in reverse, so slow drift spreads over all of them."""
    tools = spec["run"]["tool_order"]
    for profile in spec["profile"]:
        for rep in range(1, spec["run"]["repetitions"] + 1):
            order = tools if spec["run"]["order"] == "fixed" or rep % 2 else tools[::-1]
            for position, tool in enumerate(order, 1):
                yield profile, rep, position, tool


class _Checker:
    def __init__(self, base):
        self.base = base
        self.errors = []
        self.files = {}

    def err(self, path, message):
        self.errors.append(f"{path}: {message}")

    def table(self, value, path, fields):
        """Check VALUE, the table at PATH, against FIELDS: key -> (types, required, default, check).
        CHECK, if given, takes (value, path) and returns the value to keep."""
        if not isinstance(value, dict):
            self.err(path or "spec", "must be a table")
            return {}
        out = {}
        for key in value:
            if key not in fields:
                self.err(_join(path, key), "unknown key")
        for key, (types, required, default, check) in fields.items():
            p = _join(path, key)
            if key not in value:
                if required:
                    self.err(p, "is required")
                elif default is not None:
                    out[key] = default() if callable(default) else default
                continue
            v = value[key]
            if not _is(v, types):
                self.err(p, f"must be {_describe(types)}")
                continue
            out[key] = check(v, p) if check else v
        return out

    def file(self, name, relpath, path):
        full = relpath if os.path.isabs(relpath) else os.path.join(self.base, relpath)
        try:
            data = open(full, "rb").read()
        except OSError as e:
            self.err(path, f"cannot read {relpath}: {e.strerror}")
            return relpath
        self.files[name] = {"path": full, "sha256": util.sha256_file(full), "text": data.decode("utf-8", "replace")}
        return relpath

    # ---- checks

    def one_of(self, *choices):
        def check(v, p):
            if v not in choices:
                self.err(p, f"must be one of {', '.join(map(str, choices))}")
            return v
        return check

    def between(self, low, high):
        def check(v, p):
            if not low <= v <= high:
                self.err(p, f"must be between {low} and {high}")
            return v
        return check

    def matches(self, pattern, what):
        def check(v, p):
            if not pattern.fullmatch(v):
                self.err(p, f"must be {what}")
            return v
        return check

    def binary(self, v, p):
        return self.table(v, p, {
            "url": (str, True, None, self.url),
            "sha256": (str, True, None, self.matches(HEX64, "64 lowercase hex digits")),
        })

    def url(self, v, p):
        if not v.startswith(URL_SCHEMES):
            self.err(p, f"must start with one of {', '.join(URL_SCHEMES)}")
        return v

    def string_table(self, v, p):
        for key, value in v.items():
            if not isinstance(value, (str, int)) or isinstance(value, bool):
                self.err(_join(p, key), "must be a string or an integer")
        return {k: str(val) for k, val in v.items()}

    # ---- sections

    def spec(self, raw):
        spec = self.table(raw, "", {
            "name": (str, True, None, self.matches(NAME, "lowercase letters, digits and hyphens")),
            "description": (str, False, None, None),
            "corpus": (dict, True, None, self.corpus),
            "target": (dict, True, None, self.target),
            "host": (dict, False, lambda: {"sysctl": {}}, self.host),
            "profile": (list, True, None, self.profiles),
            "tool": (dict, True, None, self.tools),
            "run": (dict, False, dict, None),
        })
        spec["run"] = self.run(raw.get("run", {}), "run")
        # The tools' order as the spec lists them, which decides the run order. It is kept here
        # because the resolved spec travels as JSON with sorted keys.
        spec["run"]["tool_order"] = list(spec.get("tool", {}))
        return spec

    def corpus(self, v, p):
        c = self.table(v, p, {
            "url": (str, True, None, self.url),
            "sha256": (str, True, None, self.matches(HEX64, "64 lowercase hex digits")),
            "root": (str, False, "", None),
            "format": (str, False, "auto", self.one_of("auto", "tar", "tar.zst", "tar.gz", "tar.xz")),
        })
        root = c.get("root", "")
        if root.startswith("/") or ".." in root.split("/"):
            self.err(f"{p}.root", "must be a relative path inside the archive")
        return c

    def target(self, v, p):
        t = self.table(v, p, {
            "kind": (str, True, None, self.one_of("static", "aws", "helm")),
            "inventory": (str, False, None, None),
        })
        if t.get("kind") in ("aws", "helm"):
            self.err(f"{p}.kind", f"{t['kind']} is planned for a later phase and not supported yet")
        elif t.get("kind") == "static":
            if "inventory" not in t:
                self.err(f"{p}.inventory", "is required for a static target")
            else:
                self.file("inventory", t["inventory"], f"{p}.inventory")
        return t

    def host(self, v, p):
        h = self.table(v, p, {"sysctl": (dict, False, dict, None)})
        sysctl = {}
        for key, value in h.get("sysctl", {}).items():
            kp = f"{p}.sysctl.{key}"
            if key not in SYSCTL_KEYS:
                self.err(kp, "is not a setting the harness manages")
            elif not isinstance(value, (str, int)) or isinstance(value, bool):
                self.err(kp, "must be a string, an integer or \"inherit\"")
            else:
                sysctl[key] = str(value)
        return {"sysctl": sysctl}

    def profiles(self, v, p):
        out, names = [], set()
        if not v:
            self.err(p, "needs at least one profile")
        for i, item in enumerate(v):
            ip = f"{p}[{i}]"
            prof = self.table(item, ip, {
                "name": (str, True, None, self.matches(NAME, "lowercase letters, digits and hyphens")),
                "rtt_ms": ((int, float), True, None, self.between(0, 2000)),
                "rate_mbit": (int, False, None, self.between(1, 100000)),
                "queue_bdp": ((int, float), False, 1.0, self.between(0.05, 16)),
                "loss_ppm": (int, False, 0, self.between(0, 1000000)),
                "jitter_ms": ((int, float), False, 0, self.between(0, 1000)),
                "swap_roles": (bool, False, False, None),
            })
            if prof.get("name") in names:
                self.err(f"{ip}.name", "is used by another profile")
            names.add(prof.get("name"))
            if prof.get("jitter_ms", 0) > prof.get("rtt_ms", 0) / 2:
                self.err(f"{ip}.jitter_ms", "must be at most half of rtt_ms")
            if prof.get("swap_roles"):
                self.err(f"{ip}.swap_roles", "is not supported yet")
            if not any(e.startswith(ip + ".") for e in self.errors) and queue_bytes(prof) > MAX_QUEUE_BYTES:
                self.err(f"{ip}.queue_bdp", f"makes a queue of {queue_bytes(prof):,} bytes, more than the "
                                            f"shaper's {MAX_QUEUE_BYTES:,}")
            out.append(prof)
        return out

    def tools(self, v, p):
        out = {}
        if not v:
            self.err(p, "needs at least one tool")
        for name, settings in v.items():
            tp = f"{p}.{name}"
            if name == "p4":
                t = self.table(settings, tp, {
                    "p4d": (dict, True, None, self.binary),
                    "p4": (dict, True, None, self.binary),
                    "port": (str, False, "tcp", self.one_of("tcp", "ssl")),
                    "typemap": (str, False, "none", None),
                    "submit_threads": (int, False, 4, self.between(1, 64)),
                    "sync_threads": (int, False, 4, self.between(1, 64)),
                    "configurables": (dict, False, dict, self.string_table),
                })
                if t.get("typemap") not in (None, "none", "ue"):
                    self.file("p4.typemap", t["typemap"], f"{tp}.typemap")
            elif name == "lore":
                t = self.table(settings, tp, {
                    "lore": (dict, True, None, self.binary),
                    "loreserver": (dict, True, None, self.binary),
                    "client_env": (dict, False, dict, self.string_table),
                    "server_config": (str, False, None, None),
                })
                if "server_config" in t:
                    self.file("lore.server_config", t["server_config"], f"{tp}.server_config")
            else:
                self.err(tp, "is not a tool the harness has an adapter for (p4, lore)")
                continue
            out[name] = t
        return out

    def run(self, v, p):
        r = self.table(v, p, {
            "tests": (list, False, lambda: list(TESTS), None),
            "repetitions": (int, False, 1, self.between(1, 50)),
            "order": (str, False, "alternate", self.one_of("alternate", "fixed")),
            "settle_s": (int, False, 120, self.between(0, 3600)),
            "verify_sha256": (str, False, "first", self.one_of("first", "all", "none")),
            # The watchdog: a step with no progress (network, disk or CPU) for stall_s is stopped
            # and flagged, and so is one running longer than step_timeout_s.
            "stall_s": (int, False, 600, self.between(60, 86400)),
            "step_timeout_s": (int, False, 6 * 3600, self.between(60, 7 * 86400)),
            "calibration": (dict, False, dict, None),
        })
        tests = r.get("tests", [])
        if not tests or any(t not in TESTS for t in tests) or len(set(tests)) != len(tests):
            self.err(f"{p}.tests", f"must list tests from {', '.join(TESTS)}, each once")
        elif "fetch" in tests and "import" not in tests:
            self.err(f"{p}.tests", "fetch needs import, which creates what it fetches")
        r["tests"] = [t for t in TESTS if t in tests]
        r["calibration"] = self.table(r.get("calibration", {}), f"{p}.calibration", {
            "rtt_tolerance_ms": ((int, float), False, 1.0, self.between(0, 1000)),
            "rtt_tolerance_rel": ((int, float), False, 0.05, self.between(0, 1)),
            "tcp_min_fraction": ((int, float), False, 0.90, self.between(0, 1)),
            "disk": (bool, False, True, None),
            "enforce": (bool, False, True, None),
        })
        return r


def _join(path, key):
    return f"{path}.{key}" if path else key


def _is(value, types):
    types = types if isinstance(types, tuple) else (types,)
    if isinstance(value, bool) and bool not in types:
        return False
    return isinstance(value, types)


def _describe(types):
    types = types if isinstance(types, tuple) else (types,)
    names = {str: "a string", int: "an integer", float: "a number", bool: "true or false",
             dict: "a table", list: "a list"}
    return " or ".join(names[t] for t in types)
