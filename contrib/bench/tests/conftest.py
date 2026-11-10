"""Fixtures the tests share."""
import json

import pytest


@pytest.fixture
def make_agent(tmp_path):
    """An agent on a run directory in tmp_path, for a spec whose run section takes RUN. Its server
    is an address nothing listens on, so a host operation it reaches fails at once; it polls every
    0.2 s and gives a stopped step 2 s."""
    from bench.agent import Agent

    def make(**run):
        inv = {role: {"ip": "127.0.0.1", "user": "bench", "port": 1, "data": "/bench"} for role in ("client", "server")}
        spec = {"_inventory": inv, "run": {"stall_s": 600, "step_timeout_s": 3600, **run}}
        (tmp_path / "spec.json").write_text(json.dumps(spec))
        agent = Agent(str(tmp_path))
        agent.poll_s, agent.kill_grace_s = 0.2, 2
        return agent

    return make
