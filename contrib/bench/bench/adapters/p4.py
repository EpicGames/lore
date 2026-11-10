"""P4: import is `p4 add -f` and a parallel `p4 submit`; fetch is a parallel `p4 sync` into a new
workspace. The server starts empty each run, with the spec's configurables, an optional typemap,
security level 4 and one super user."""
import os
import re
import secrets
import subprocess

# Perforce's published Unreal Engine typemap (help.perforce.com, "Step 2: Set up a typemap").
UE_TYPEMAP = "TypeMap:\n" + "".join(
    [f"\tbinary+w //....{ext}\n" for ext in ("exe", "dll", "lib", "app", "dylib", "stub", "ipa")]
    + [f"\tbinary+l //....{ext}\n" for ext in ("uasset", "umap", "upk", "udk", "ubulk")]
    + ["\tbinary+wS2 //..._BuiltData.uasset\n"])
DEPOT = "//depot/bench"


class P4:
    name = "p4"

    def __init__(self, ctx):
        self.ctx = ctx
        self.s = ctx.settings
        self.p4 = ctx.client_bins["p4"]
        # Not 32 hex digits, which P4 takes for a prehashed password and refuses; mixed case and a
        # symbol meet the strength rules of security level 4.
        self.password = f"Bench-{secrets.token_hex(12)}"
        self.dir = os.path.join(ctx.server_data, "srv", "p4")
        self.server_pidfile = os.path.join(self.dir, "p4d.pid")
        port = f"{ctx.server_ip}:1666"
        self.env = dict(os.environ, P4PORT=f"ssl:{port}" if self.s["port"] == "ssl" else port, P4USER="bench",
                        P4TICKETS=os.path.join(ctx.run_dir, "p4tickets"), P4TRUST=os.path.join(ctx.run_dir, "p4trust"),
                        P4ENVIRO="/dev/null", P4CONFIG="")

    def typemap(self):
        if self.s["typemap"] == "ue":
            return UE_TYPEMAP
        if self.s["typemap"] == "none":
            return None
        return self.ctx.files["p4.typemap"]["text"]

    def reset_server(self):
        return self.ctx.server.op("p4-reset", input={
            "dir": self.dir, "p4d": self.ctx.server_bins["p4d"], "p4": self.ctx.server_bins["p4"],
            "port": self.s["port"], "password": self.password, "configurables": self.s["configurables"],
            "typemap": self.typemap()}, timeout=300)

    def _p4(self, *args, input=None, cwd=None):
        env = dict(self.env, PWD=cwd) if cwd else self.env  # P4 resolves relative paths against $PWD
        res = subprocess.run([self.p4, *args], input=input, text=True, capture_output=True, env=env, cwd=cwd,
                             timeout=900)
        if res.returncode != 0:
            raise RuntimeError(f"p4 {' '.join(args)}: {res.stderr.strip() or res.stdout.strip()}")
        return res.stdout

    def _login(self):
        if self.s["port"] == "ssl":
            self._p4("trust", "-y", "-f")
        self._p4("login", input=self.password + "\n")

    def _workspace(self, name, root):
        spec = self._p4("client", "-o", name)
        spec = re.sub(r"^Root:.*$", f"Root:\t{root}", spec, flags=re.M)
        spec = spec.split("\nView:")[0] + f"\nView:\n\t{DEPOT}/... //{name}/...\n"
        self._p4("client", "-i", input=spec)
        self.env["P4CLIENT"] = name

    def setup_import(self):
        self._login()
        self._workspace(f"bench-import-{self.ctx.rep}", self.ctx.corpus["root"])

    def import_steps(self):
        root, n = self.ctx.corpus["root"], self.s["submit_threads"]
        return [("p4-add", [self.p4, "-Ztrack", "-x", self.ctx.corpus["filelist"], "add", "-f"], root, self.env),
                ("p4-submit", [self.p4, "-Ztrack", "submit", "-d", "New files", f"--parallel=threads={n}"], root, self.env)]

    def setup_fetch(self):
        os.makedirs(self.ctx.work, exist_ok=True)
        self._workspace(f"bench-fetch-{self.ctx.rep}", self.ctx.work)

    def fetch_steps(self):
        return [("p4-sync", [self.p4, "-Ztrack", "sync", f"--parallel=threads={self.s['sync_threads']}"],
                 self.ctx.work, self.env)]

    def expected_files(self, paths):
        return paths

    def collect(self, phase):
        out = {"server_root_bytes": self.ctx.server.op("du", os.path.join(self.dir, "root"))["bytes"]}
        if phase == "import":
            sizes = self._p4("-ztag", "sizes", "-s", f"{DEPOT}/...")
            m = re.search(r"\.\.\. fileSize (\d+)", sizes)
            out["depot_file_bytes"] = int(m.group(1)) if m else None
            m = re.search(r"\.\.\. fileCount (\d+)", sizes)
            out["depot_files"] = int(m.group(1)) if m else None
            types = {}
            for t in re.findall(r"^\.\.\. headType (\S+)", self._p4("-ztag", "fstat", "-T", "headType", f"{DEPOT}/..."), re.M):
                types[t] = types.get(t, 0) + 1
            out["file_types"] = types
        return out

    def server_log(self):
        return os.path.join(self.dir, "logs", "log")

    def version(self):
        out = subprocess.run([self.p4, "-V"], capture_output=True, text=True, timeout=60).stdout
        return re.search(r"^Rev\. (.*)$", out, re.M).group(1)
