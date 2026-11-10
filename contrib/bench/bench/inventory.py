"""A static target's inventory: the two hosts the harness uses, in TOML.

    [client]                       # and [server], the same keys
    ip = "10.0.0.1"                # the address the other host sees this one at
    ssh = "bench@10.0.0.1:22"      # how the operator reaches it; the client reaches the server
                                   # at the server's ip and this port
    jump = "me@gateway"            # optional: an SSH jump host for the operator
    data = "/bench"                # a directory the harness may fill and TRIM
    rated = { read_mbs = 7200, write_mbs = 6600 }   # the data disk's rated speeds, MB/s; optional
                                   # cache_gb and after_cache_mbs describe an SLC cache
    notes = "16-vCPU VM"           # optional, recorded with the results
"""
import re
import tomllib

IPV4 = re.compile(r"(25[0-5]|2[0-4]\d|1?\d?\d)(\.(25[0-5]|2[0-4]\d|1?\d?\d)){3}")
SSH = re.compile(r"(?P<user>[a-z_][a-z0-9_-]*)@(?P<host>[A-Za-z0-9.-]+)(:(?P<port>\d{1,5}))?")


class InventoryError(Exception):
    def __init__(self, errors):
        super().__init__("\n".join(errors))
        self.errors = errors


def parse(text, path="inventory"):
    try:
        raw = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise InventoryError([f"{path}: {e}"]) from None
    errors, out = [], {}
    for key in raw:
        if key not in ("client", "server"):
            errors.append(f"{key}: unknown key")
    for role in ("client", "server"):
        h = raw.get(role)
        if not isinstance(h, dict):
            errors.append(f"{role}: is required and must be a table")
            continue
        for key in h:
            if key not in ("ip", "ssh", "jump", "data", "rated", "notes"):
                errors.append(f"{role}.{key}: unknown key")
        host = {"role": role, "notes": str(h.get("notes", "")), "jump": h.get("jump")}
        if not IPV4.fullmatch(str(h.get("ip", ""))):
            errors.append(f"{role}.ip: must be an IPv4 address")
        host["ip"] = h.get("ip")
        m = SSH.fullmatch(str(h.get("ssh", "")))
        if not m:
            errors.append(f"{role}.ssh: must be user@host or user@host:port")
        else:
            host.update(user=m["user"], host=m["host"], port=int(m["port"] or 22))
        data = h.get("data", "")
        if not isinstance(data, str) or not data.startswith("/") or data.rstrip("/") == "":
            errors.append(f"{role}.data: must be an absolute path other than /")
        host["data"] = str(data).rstrip("/")
        rated = h.get("rated", {})
        if not isinstance(rated, dict) or any(k not in ("read_mbs", "write_mbs", "cache_gb", "after_cache_mbs")
                                               or not isinstance(v, (int, float)) for k, v in rated.items()):
            errors.append(f"{role}.rated: must be a table of read_mbs, write_mbs, cache_gb, after_cache_mbs")
            rated = {}
        host["rated"] = rated
        out[role] = host
    if errors:
        raise InventoryError(errors)
    return out
