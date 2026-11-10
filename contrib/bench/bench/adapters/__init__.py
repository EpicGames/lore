"""Tool adapters. Each knows how to reset its server, and what an import and a fetch are for its
tool. Steps are (name, argv, cwd, env) tuples the runner times under GNU time; setup before a phase
is not timed.

    reset_server()            a fresh, empty server; returns what it reports
    server_pidfile            the server process's pid file on the server, for its monitor
    setup_import(), import_steps()
    setup_fetch(), fetch_steps()
    expected_files(paths)     what a fetch should produce from the corpus file list
    collect(phase)            sizes and anything else worth keeping after PHASE
    version()                 the client's version string
"""
from .lore import Lore
from .p4 import P4

ADAPTERS = {"p4": P4, "lore": Lore}


class Context:
    """What an adapter needs from the run: the hosts, binaries, corpus and directories."""

    def __init__(self, server, server_ip, server_data, client_bins, server_bins, corpus, run_dir, work, rep,
                 settings, files, server_run_dir):
        self.server, self.server_ip, self.server_data = server, server_ip, server_data
        self.client_bins, self.server_bins = client_bins, server_bins
        self.corpus, self.run_dir, self.work, self.rep = corpus, run_dir, work, rep
        self.settings, self.files, self.server_run_dir = settings, files, server_run_dir


def make(name, ctx):
    return ADAPTERS[name](ctx)
