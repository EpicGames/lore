"""Running commands on a host: this one, or another over SSH with a shared connection."""
import json
import os
import shlex
import subprocess


class HostError(Exception):
    pass


class Host:
    """A benchmark host. With LOCAL, commands run here; otherwise over SSH as USER@ADDRESS:PORT,
    through JUMP if given. HARNESS is where the harness is installed on the host."""

    def __init__(self, role, address, user, port=22, jump=None, key=None, known_hosts=None,
                 harness=None, local=False):
        self.role, self.address, self.user, self.port = role, address, user, port
        self.jump, self.key, self.known_hosts, self.harness, self.local = jump, key, known_hosts, harness, local

    def ssh_options(self):
        opts = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "-o", "ServerAliveInterval=30",
                "-o", "ControlMaster=auto", "-o", "ControlPath=/tmp/bench-ssh-%C", "-o", "ControlPersist=600",
                "-o", "StrictHostKeyChecking=accept-new"]
        if self.known_hosts:
            opts += ["-o", f"UserKnownHostsFile={self.known_hosts}"]
        if self.key:
            opts += ["-i", self.key]
        if self.jump:
            opts += ["-o", f"ProxyJump={self.jump}"]
        return opts

    def target(self):
        return f"{self.user}@{self.address}"

    def run(self, command, check=True, input=None, timeout=900, capture=True):
        """Run COMMAND, a shell string or an argument list, and return the CompletedProcess. No
        command may hang a run: it times out after TIMEOUT seconds, 15 minutes unless the caller
        gives another; None, for a foreground command the operator watches, waits for ever."""
        if isinstance(command, (list, tuple)):
            command = shlex.join(command)
        if self.local:
            argv = ["bash", "-c", command]
        else:
            argv = ["ssh", *self.ssh_options(), "-p", str(self.port), self.target(), command]
        try:
            res = subprocess.run(argv, input=input, text=True, timeout=timeout,
                                 stdout=subprocess.PIPE if capture else None,
                                 stderr=subprocess.PIPE if capture else None)
        except subprocess.TimeoutExpired:
            raise HostError(f"{self.role}: `{command}` still running after {timeout} s") from None
        if check and res.returncode != 0:
            raise HostError(f"{self.role}: `{command}` exited {res.returncode}: {(res.stderr or '').strip()[-2000:]}")
        return res

    def op(self, name, *args, input=None, timeout=900):
        """Run the host operation NAME (bench/hostops.py) and return its JSON result. No operation
        may hang a run: each has a timeout, 15 minutes unless the caller gives another."""
        cmd = f"cd {shlex.quote(self.harness)} && python3 -m bench hostops {shlex.join([name, *map(str, args)])}"
        res = self.run(cmd, input=json.dumps(input) if input is not None else None, timeout=timeout)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def put(self, local_path, remote_path, recursive=False):
        if self.local:
            subprocess.run(["cp", "-a" if recursive else "-p", local_path, remote_path], check=True)
            return
        argv = ["scp", "-q", *self.ssh_options(), "-P", str(self.port)]
        if recursive:
            argv.append("-r")
        subprocess.run([*argv, local_path, f"{self.target()}:{remote_path}"], check=True, timeout=3600)

    def get(self, remote_path, local_path, recursive=False):
        if self.local:
            subprocess.run(["cp", "-a" if recursive else "-p", remote_path, local_path], check=True)
            return
        argv = ["scp", "-q", *self.ssh_options(), "-P", str(self.port)]
        if recursive:
            argv.append("-r")
        subprocess.run([*argv, f"{self.target()}:{remote_path}", local_path], check=True, timeout=3600)


    def get_dir(self, remote_dir, local_dir, compress=False, timeout=3600):
        """Copy the contents of REMOTE_DIR into LOCAL_DIR, through a tar stream, in at most TIMEOUT
        seconds."""
        os.makedirs(local_dir, exist_ok=True)
        if self.local:
            subprocess.run(["cp", "-a", remote_dir.rstrip("/") + "/.", local_dir], check=True)
            return
        z = "z" if compress else ""
        src = subprocess.Popen(["ssh", *self.ssh_options(), "-p", str(self.port), self.target(),
                                f"tar -C {shlex.quote(remote_dir)} -c{z}f - ."], stdout=subprocess.PIPE)
        try:
            subprocess.run(["tar", "-C", local_dir, f"-x{z}f", "-"], stdin=src.stdout, check=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            src.kill()
            raise HostError(f"{self.role}: copying {remote_dir} took more than {timeout} s") from None
        finally:
            src.stdout.close()
        if src.wait(timeout=60) != 0:
            raise HostError(f"{self.role}: copying {remote_dir} failed")


def from_inventory(entry, harness, key=None, known_hosts=None, via_peer=False, local=False):
    """A Host for an inventory ENTRY. VIA_PEER: as the other host reaches it (its ip, no jump),
    rather than as the operator does."""
    address = entry["ip"] if via_peer else entry["host"]
    return Host(entry["role"], address, entry["user"], entry["port"], None if via_peer else entry.get("jump"),
                key, known_hosts, harness, local)


def harness_dir(entry):
    return os.path.join(entry["data"], "harness")
