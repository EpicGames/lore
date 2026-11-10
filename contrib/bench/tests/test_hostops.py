"""Host counters and the host's description."""
import os

from bench import hostops, util
from bench.hostops import parse_snmp, softnet_drops, udp_counters

# /proc/net/snmp after a socket's full receive buffer dropped 3 datagrams, which Linux charges to
# both InErrors and RcvbufErrors, and 2 more were dropped for a bad checksum.
SNMP = """\
Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs OutRsts InCsumErrors
Tcp: 1 200 120000 -1 10 5 0 0 2 1000 900 7 0 0 0
Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors InCsumErrors IgnoredMulti MemErrors
Udp: 5000 1 5 4000 3 0 2 0 0
"""


def test_a_receive_buffer_drop_is_counted_once():
    snmp = parse_snmp(SNMP)
    assert snmp[("Tcp", "RetransSegs")] == 7
    assert udp_counters(snmp) == {"udp_in_errors": 5, "udp_rcvbuf_errors": 3, "udp_sndbuf_errors": 0}


def test_softnet_backlog_drops():
    # One row per CPU, in hexadecimal; the second column is what the receive backlog dropped.
    text = "0004a2c1 00000002 00000000 00000000\n0003f001 00000010 00000001 00000000\n"
    assert softnet_drops(text) == 18


def test_the_fingerprint_hashes_the_harnesss_helper_and_a_missing_one_as_empty(tmp_path):
    assert hostops.HARNESS_HELPER == os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                                  "host", "bench-helper")
    assert hostops._sha256(hostops.HARNESS_HELPER) == util.sha256_file(hostops.HARNESS_HELPER)
    assert hostops._sha256(str(tmp_path / "missing")) == ""
