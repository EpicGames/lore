"""Helpers shared by the harness's modules."""
import hashlib
import json
import os
import re
import sys
import time


def sha256_file(path, chunk=1 << 22):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(chunk):
            h.update(block)
    return h.hexdigest()


def canonical(obj):
    """OBJ as JSON bytes that don't depend on key order or formatting."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(obj):
    return hashlib.sha256(canonical(obj)).hexdigest()


def write_json(path, obj):
    """Write OBJ to PATH atomically, so a reader never sees half a file."""
    tmp = f"{path}.tmp"
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=2, sort_keys=True)
        f.write("\n")
    os.replace(tmp, path)


def read_json(path):
    with open(path) as f:
        return json.load(f)


def gnu_time(path):
    """The figures GNU time -v wrote to PATH. "exit" is None when it wrote none, as when it was
    stopped before its command ended."""
    try:
        text = open(path).read()
    except FileNotFoundError:
        text = ""

    def field(label, conv=float, default=None):
        m = re.search(re.escape(label) + r".*?: (\S+)", text)
        return conv(m.group(1)) if m else default

    wall = 0.0
    for part in field("Elapsed (wall clock) time", str, "0").split(":"):
        wall = wall * 60 + float(part)
    status = re.search(r"Exit status: (\d+)", text)
    signal = re.search(r"Command terminated by signal (\d+)", text)
    return {
        "wall_s": wall,
        "user_s": field("User time (seconds)", float, 0.0),
        "sys_s": field("System time (seconds)", float, 0.0),
        "max_rss_bytes": field("Maximum resident set size (kbytes)", int, 0) * 1024,
        # A command a signal ended has both lines, and "Exit status: 0".
        "exit": 128 + int(signal.group(1)) if signal else (int(status.group(1)) if status else None),
    }


def mmss(seconds):
    seconds = float(seconds)
    if seconds >= 3600:
        return f"{int(seconds // 3600)}:{int(seconds % 3600 // 60):02d}:{seconds % 60:04.1f}"
    return f"{int(seconds // 60)}:{seconds % 60:04.1f}"


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


class Log:
    """Timestamped lines to a file and to standard error."""

    def __init__(self, path=None):
        self.path = path

    def __call__(self, message):
        line = f"[{time.strftime('%H:%M:%S')}] {message}"
        print(line, file=sys.stderr, flush=True)
        if self.path:
            with open(self.path, "a") as f:
                f.write(line + "\n")


def sort_key(path):
    """Byte order, as LC_ALL=C sort uses, for paths that may not be valid UTF-8."""
    return path.encode("utf-8", "surrogateescape")
