"""Spec checking and identity. Run from contrib/bench: python3 -m pytest tests"""
import os
import shutil
import subprocess

import pytest

from bench import spec as specmod

HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLES = os.path.join(os.path.dirname(HERE), "examples")


@pytest.fixture
def example(tmp_path):
    for name in ("workstation-vms.toml", "workstation-vms.hosts.toml"):
        shutil.copy(os.path.join(EXAMPLES, name), tmp_path / name)
    return tmp_path / "workstation-vms.toml"


def errors_of(path):
    with pytest.raises(specmod.SpecError) as e:
        specmod.load(path)
    return e.value.errors


def test_examples_validate(example):
    spec = specmod.load(example)
    assert spec["name"] == "workstation-vms-30ms-5g"
    assert spec["_inventory"]["server"]["port"] == 2222
    assert [list(m)[1:] for m in specmod.matrix(spec)] == [[1, 1, "p4"], [1, 2, "lore"]]


def test_the_cloud_inventory_example_parses():
    from bench import inventory
    inv = inventory.parse(open(os.path.join(EXAMPLES, "cloud.hosts.toml")).read())
    assert (inv["server"]["ip"], inv["server"]["user"], inv["server"]["port"]) == ("10.0.1.20", "ubuntu", 22)


def test_run_order_survives_the_json_round_trip(example):
    import json
    spec = json.loads(json.dumps(specmod.load(example), sort_keys=True))
    assert [m[3] for m in specmod.matrix(spec)] == ["p4", "lore"]


def test_reformatting_keeps_the_identity(example):
    before = specmod.load(example)["_sha256"]
    text = example.read_text()
    reordered = text.replace('rtt_ms = 30\nrate_mbit = 4920', 'rate_mbit = 4920\n   rtt_ms   =   30')
    assert reordered != text
    example.write_text("# a comment\n" + reordered)
    assert specmod.load(example)["_sha256"] == before


def test_a_referenced_file_is_part_of_the_identity(example):
    before = specmod.load(example)["_sha256"]
    hosts = example.parent / "workstation-vms.hosts.toml"
    hosts.write_text(hosts.read_text().replace('notes = "16-vCPU', 'notes = "Sixteen-vCPU', 1))
    assert specmod.load(example)["_sha256"] != before


@pytest.mark.parametrize("old,new,message", [
    ('rtt_ms = 30', 'rtt_ms = 30\nbogus = 1', "profile[0].bogus: unknown key"),
    ('rate_mbit = 4920', 'rate_mbit = 0', "profile[0].rate_mbit: must be between 1 and 100000"),
    ('repetitions = 1', 'repetitions = "one"', "run.repetitions: must be an integer"),
    ('tests = ["import", "fetch"]', 'tests = ["fetch"]', "run.tests: fetch needs import"),
    ('kind = "static"', 'kind = "aws"', "target.kind: aws is planned"),
    ('"vm.dirty_ratio" = 20', '"vm.overcommit_memory" = 1', "host.sysctl.vm.overcommit_memory: is not a setting"),
    ('port = "tcp"', 'port = "udp"', "tool.p4.port: must be one of tcp, ssl"),
    # 4,920 Mbit/s for 2 s is a 1.2 GB queue, more than the shaper takes.
    ('rtt_ms = 30', 'rtt_ms = 2000', "profile[0].queue_bdp: makes a queue"),
    ('kind = "static"', 'kind = "static"\npreset = "x"', "target.preset: unknown key"),
])
def test_errors_name_the_key(example, old, new, message):
    text = example.read_text()
    assert old in text
    example.write_text(text.replace(old, new, 1))
    assert any(m.startswith(message) for m in errors_of(example)), errors_of(example)


def test_missing_corpus_hash(example):
    text = example.read_text()
    example.write_text("\n".join(l for l in text.splitlines() if not l.startswith("sha256 = ")))
    assert "corpus.sha256: is required" in errors_of(example)


def test_series_key_ignores_the_binaries_but_not_settings(example):
    spec = specmod.load(example)
    prof = spec["profile"][0]
    key = specmod.series_key(spec, prof, "import", "lore", "corpus", {"c": 1})
    spec["tool"]["lore"]["lore"]["sha256"] = "0" * 64
    assert specmod.series_key(spec, prof, "import", "lore", "corpus", {"c": 1}) == key
    spec["tool"]["lore"]["client_env"]["LORE_MAX_CONNECTIONS"] = "8"
    assert specmod.series_key(spec, prof, "import", "lore", "corpus", {"c": 1}) != key


def test_cli_validate(example):
    res = subprocess.run(["python3", "-m", "bench", "validate", str(example)], cwd=os.path.dirname(HERE),
                         capture_output=True, text=True)
    assert res.returncode == 0, res.stderr
    assert "2 runs" in res.stdout
