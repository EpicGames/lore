"""Flags on a phase's results."""
from bench.agent import flags


def phase(server, **extra):
    base = {"steps": [{"exit": 0}], "wire": {"client": {}, "server": {}},
            "monitor": {"client": {"cpu_steal_pct": 0}, "server": {"cpu_steal_pct": 0, **server}}}
    base.update(extra)
    return base


def test_io_wait_on_a_busy_disk_is_not_other_load():
    # A fetch from a slow server disk: 9 cores busy, most of it I/O wait and kernel work for the
    # server's own reads; user space is all the server's processes.
    ph = phase({"seconds": 88, "cpu_busy_cores": 9.0, "cpu_user_cores": 1.4,
                "users": {"cpu_s": 254.0, "usr_s": 120.0}})
    assert "busy-server" not in flags(ph)


def test_another_process_in_user_space_is_other_load():
    ph = phase({"seconds": 100, "cpu_busy_cores": 4.0, "cpu_user_cores": 3.0,
                "users": {"cpu_s": 150.0, "usr_s": 120.0}})
    assert "busy-server" in flags(ph)


def test_failed_step_and_verification():
    ph = phase({"seconds": 1, "cpu_user_cores": 0, "users": {"usr_s": 0}},
               steps=[{"exit": 1}], verify={"files_match": True, "sha256_match": False})
    assert flags(ph) == ["step-failed", "verify-failed"]


def test_receive_drops_and_throttling():
    ph = phase({"seconds": 1, "cpu_user_cores": 0, "users": {"usr_s": 0}},
               wire={"client": {"rx_missed": 3}, "server": {"ena": {"bw_in_allowance_exceeded": 2}}})
    assert flags(ph) == ["nic-drops", "throttled"]
    ph = phase({"seconds": 1, "cpu_user_cores": 0, "users": {"usr_s": 0}},
               wire={"client": {"softnet_drops": 1}, "server": {}})
    assert flags(ph) == ["nic-drops"]


def test_frames_nothing_handles_are_not_nic_drops():
    # rx_dropped also counts frames of protocols nothing on the host handles, such as a switch's LLDP.
    ph = phase({"seconds": 1, "cpu_user_cores": 0, "users": {"usr_s": 0}},
               wire={"client": {"rx_dropped": 40, "rx_missed": 0}, "server": {"rx_dropped": 7}})
    assert "nic-drops" not in flags(ph)


def test_a_truncated_file_fails_verification():
    ph = phase({"seconds": 1, "cpu_user_cores": 0, "users": {"usr_s": 0}},
               verify={"files_match": True, "sizes_match": False})
    assert flags(ph) == ["verify-failed"]
