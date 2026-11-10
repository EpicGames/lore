"""Lore: import is `lore stage`, `lore commit` and `lore push` in the corpus tree; fetch is
`lore clone` into a new directory. Each phase gets fresh configuration and data directories, as on a
new machine. The server starts with an empty store each run."""
import os
import subprocess

PORT = 41337


class Lore:
    name = "lore"

    def __init__(self, ctx):
        self.ctx = ctx
        self.s = ctx.settings
        self.lore = ctx.client_bins["lore"]
        self.dir = os.path.join(ctx.server_data, "srv", "lore")
        self.server_pidfile = os.path.join(self.dir, "loreserver.pid")
        self.url = f"lore://{ctx.server_ip}:{PORT}/bench"
        self.env = None

    def _env(self, phase):
        x = os.path.join(self.ctx.run_dir, f"xdg-{phase}")
        self.env = dict(os.environ, XDG_CONFIG_HOME=f"{x}/config", XDG_DATA_HOME=f"{x}/data",
                        XDG_CACHE_HOME=f"{x}/cache", XDG_STATE_HOME=f"{x}/state", **self.s["client_env"])

    def reset_server(self):
        server_config = self.ctx.files.get("lore.server_config", {}).get("text")
        return self.ctx.server.op("lore-reset", input={
            "dir": self.dir, "loreserver": self.ctx.server_bins["loreserver"], "server_config": server_config},
            timeout=300)

    def setup_import(self):
        self._env("import")
        res = subprocess.run([self.lore, "repository", "create", self.url], cwd=self.ctx.corpus["root"],
                             env=self.env, capture_output=True, text=True, timeout=900)
        if res.returncode != 0:
            raise RuntimeError(f"lore repository create: {res.stderr.strip() or res.stdout.strip()}")

    def import_steps(self):
        root = self.ctx.corpus["root"]
        return [("lore-stage", [self.lore, "stage", "--targets", self.ctx.corpus["filelist"]], root, self.env),
                ("lore-commit", [self.lore, "commit", "New files"], root, self.env),
                ("lore-push", [self.lore, "push"], root, self.env)]

    def setup_fetch(self):
        self._env("fetch")
        os.makedirs(os.path.dirname(self.ctx.work), exist_ok=True)

    def fetch_steps(self):
        return [("lore-clone", [self.lore, "clone", self.url, self.ctx.work], os.path.dirname(self.ctx.work), self.env)]

    def expected_files(self, paths):
        """Lore refuses, by design, a path that differs only in case from one staged before it, so
        a fetch has the corpus less those."""
        seen, out = set(), []
        for p in paths:
            if p.lower() not in seen:
                out.append(p)
            seen.add(p.lower())
        return out

    def collect(self, phase):
        out = {"server_store_bytes": self.ctx.server.op("du", os.path.join(self.dir, "store"))["bytes"]}
        local = os.path.join(self.ctx.corpus["root"] if phase == "import" else self.ctx.work, ".lore")
        out["client_store_bytes"] = int(subprocess.run(["du", "-s", "--block-size=1", local], capture_output=True, timeout=900,
                                                       text=True).stdout.split()[0]) if os.path.exists(local) else 0
        return out

    def server_log(self):
        return self.ctx.server.op("latest-log", os.path.join(self.dir, "logs"), r"server-.*\.log")["path"]

    def version(self):
        return subprocess.run([self.lore, "--version"], capture_output=True, text=True, timeout=60).stdout.strip()
