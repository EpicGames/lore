"""bench-helper accepts only its documented forms. Run as an ordinary user, it prints what it would
do instead of doing it, which these tests use."""
import os
import subprocess

import pytest

HELPER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "host", "bench-helper")


@pytest.fixture
def env(tmp_path):
    conf = tmp_path / "helper.conf"
    conf.write_text("DEV=eth0\nDATA=/bench\n")
    return dict(os.environ, BENCH_HELPER_CONF=str(conf), BENCH_HELPER_DRYRUN="1")


def helper(env, *args):
    return subprocess.run([HELPER, *args], env=env, capture_output=True, text=True)


def test_shape(env):
    res = helper(env, "shape", "10.0.0.2", "15000", "5000", "18750000")
    assert res.returncode == 0, res.stderr
    assert "netem delay 15000us limit 1000000" in res.stdout
    assert "tbf rate 5000mbit burst 256kb limit 18750000" in res.stdout
    assert "match ip dst 10.0.0.2/32 flowid 1:1" in res.stdout


def test_shape_caps_nothing_but_the_peer(env):
    # A rate-limited root, as HTB's was at 10 Gbit/s, would cap a faster card's other traffic, and
    # a profile without a rate limit.
    out = helper(env, "shape", "10.0.0.2", "15000", "0", "0").stdout
    assert "root handle 1: prio bands 2" in out
    assert "parent 1:1 handle 10: netem" in out and "parent 1:2 handle 30: fq_codel" in out
    assert "htb" not in out and "rate" not in out


def test_the_helper_runs_the_systems_commands(env, tmp_path):
    # Without secure_path in sudoers, the caller's PATH would reach root: a tc of the caller's own
    # would run in the helper's place.
    fake = tmp_path / "bin"
    fake.mkdir()
    (fake / "tc").write_text("#!/bin/sh\necho FAKE\n")
    (fake / "tc").chmod(0o755)
    env = dict(env, PATH=f"{fake}:{env['PATH']}", BENCH_HELPER_DRYRUN="")
    res = subprocess.run([HELPER, "unshape"], env=env, capture_output=True, text=True)
    assert "FAKE" not in res.stdout


def test_shape_without_rate_has_no_tbf_and_loss_in_percent(env):
    res = helper(env, "shape", "10.0.0.2", "15000", "0", "0", "100", "500")
    assert res.returncode == 0, res.stderr
    assert "tbf" not in res.stdout
    assert "netem delay 15000us 500us loss 0.0100% limit 1000000" in res.stdout


def test_small_queue_is_raised_to_the_burst(env):
    assert "limit 262144" in helper(env, "shape", "10.0.0.2", "100", "1000", "1000").stdout


@pytest.mark.parametrize("args", [
    [], ["reboot"], ["shape"], ["shape", "10.0.0.256", "1", "1", "1"], ["shape", "10.0.0", "1", "1", "1"],
    ["shape", "10.0.0.2;id", "1", "1", "1"], ["shape", "10.0.0.2", "-1", "1", "1"],
    ["shape", "10.0.0.2", "1000001", "1", "1"], ["shape", "10.0.0.2", "1e3", "1", "1"],
    ["shape", "10.0.0.2", "100", "1", "1", "0", "101"], ["shape", "10.0.0.2", "1", "1", "1", "0", "0", "extra"],
    ["shape", "10.0.0.2", "99999999999", "1", "1"], ["unshape", "now"], ["trim", "/"],
    ["sysctl", "kernel.core_pattern", "|/bin/sh"], ["sysctl", "vm.dirty_ratio", "20;id"],
    ["sysctl", "net.ipv4.tcp_congestion_control", "Cubic"], ["sysctl", "vm.dirty_ratio"],
])
def test_rejects(env, args):
    res = helper(env, *args)
    assert res.returncode == 2, (args, res.stdout)
    assert res.stdout == ""


def test_sysctl(env):
    assert helper(env, "sysctl", "net.ipv4.tcp_rmem", "4096 131072 33554432").stdout.strip() == \
        "sysctl -q -w net.ipv4.tcp_rmem=4096 131072 33554432"
    assert helper(env, "sysctl", "net.ipv4.tcp_congestion_control", "bbr").returncode == 0
