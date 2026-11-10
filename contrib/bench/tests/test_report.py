"""Reports."""
import os

from bench import report, util


def result(binaries, wall, flags=(), day=1):
    return {"tool": {"name": "lore", "version": "1.0.0+local", "binaries": {"lore": binaries}},
            "profile": {"name": "p"}, "started": f"2026-11-0{day}T00:00:00Z",
            "phases": {"fetch": {"series": "s" * 64, "flags": list(flags), "wall_s": wall, "wire_bytes": {"x": 1},
                                 "client_cpu_s": 1.0, "client_peak_rss_bytes": 1}}}


def test_the_calibration_shows_the_unshaped_round_trip_measured_for_its_profile():
    cal = {"rtt_idle_ms": {"median": 5.1}, "rtt_loaded_ms": {}, "base_rtt_ms": {"median": 1.2}, "delay_us_each_way": 1900}
    assert "unshaped 1.20 ms, 1.90 ms added each way" in report.calibration_table(cal, {"median": 0.5})
    # A calibration from before the base was measured per profile shows the run's.
    assert "unshaped 0.50 ms)" in report.calibration_table({"rtt_idle_ms": {"median": 30.6}, "rtt_loaded_ms": {}},
                                                           {"median": 0.5})


def test_trends_tell_builds_apart_and_leave_failed_runs_out(tmp_path):
    # Two unstamped builds share a version string; a failed run's 3 s is no time at all.
    for i, r in enumerate((result("a" * 64, 100, day=1), result("b" * 64, 200, day=2),
                           result("c" * 64, 3, ["step-failed"], day=3))):
        d = tmp_path / "p" / f"lore-r{i + 1}"
        os.makedirs(d)
        util.write_json(str(d / "result.json"), r)
    page = report.trend([str(tmp_path)])
    assert page.count("<tr><td>") == 3
    assert "1:40.0" in page and "3:20.0" in page
    assert "0:03.0" not in page
    assert "<svg" in page


def test_failed_phases_have_no_time_in_the_matrix():
    phases = [{"flags": ["stalled"]}, {"flags": ["steal"]}, {"flags": []}]
    assert report.usable(phases) == phases[1:]


def test_charts_of_long_phases():
    assert "<svg" in report.bars([("P4", 20 * 3600, "#000")])
    assert "<svg" in report.timeline([("P4", "#000", [1.0] * 40000)], None)


def phase(wall, flags=()):
    """A phase with every figure the matrix shows."""
    disk = {"written_bytes": 1, "util_avg": 1.0}
    return {"wall_s": wall, "flags": list(flags), "steps": [{"name": "lore-clone", "wall_s": wall}],
            "wire_bytes": {"client_to_server": 1, "server_to_client": 1}, "wire": {"client": {}, "server": {}},
            "client_cpu_s": 1.0, "client_peak_rss_bytes": 1, "flush_s": {"client": 0.0, "server": 0.0},
            "monitor": {"client": {"disk": disk, "dirty_peak_bytes": 1},
                        "server": {"disk": disk, "mem_used_rise_bytes": 1, "users": {"cpu_s": 1.0}}}}


def test_the_matrix_leaves_a_failed_run_out_of_its_chart_and_figures(tmp_path):
    results = [{"tool": {"name": "lore"}, "phases": {"fetch": p}, "_dir": str(tmp_path)}
               for p in (phase(3, ["stalled"]), phase(100))]
    fps = {"client": {"nic": {"dev": "eth0"}}}
    page = report.test_section("fetch", {"name": "p"}, results, ["lore"], {"bytes": 1}, {"client": {}, "server": {}}, fps)
    assert "1:40.0" in page and "0:03.0" not in page
    assert "stalled" in page  # listed under the flags


def test_an_axis_of_days_has_a_step_of_whole_days():
    assert report.tick(10 * 86400, 8) % 86400 == 0
    assert 10 * 86400 / report.tick(10 * 86400, 8) <= 8
