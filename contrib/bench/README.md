# bench: reproducible benchmarks for version control tools

bench measures how long version control tools take to import a large tree into a server and to
fetch it back, over a network link with a set round trip and bandwidth. It runs P4 and Lore today.

One spec file describes an experiment: the corpus, the two hosts, their kernel settings, the network
profiles, the tools with their exact builds and settings, the tests and the repetitions. The harness:

1. prepares both hosts the same way;
2. shapes the link between them and checks the link and the disks against the spec;
3. runs every test for every tool, and verifies every fetched tree against the corpus;
4. writes one result per run.

Results from the same spec on the same kind of hosts form a series, so a newer build of a tool can
be compared with earlier ones.

The hosts can be workstations or servers, bare metal or VMs, on your desk or in the cloud. The
harness only needs SSH access to two Linux machines. `vm/` makes matching VMs on any Linux host that
has Docker and KVM.

For a first run, follow [QUICKSTART.md](QUICKSTART.md). It goes from nothing to a report on two
Linux machines, two VMs or two cloud instances.

- [How it fits together](#how-it-fits-together)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Step by step](#step-by-step)
- [Local machines](#local-machines)
- [Cloud instances](#cloud-instances)
- [The spec](#the-spec)
- [What a run does](#what-a-run-does)
- [Watching a run](#watching-a-run)
- [Results and reports](#results-and-reports)
- [Tests](#tests)
- [Limitations](#limitations)

## How it fits together

There are three machines:

- **The operator's machine** is where you run `python3 -m bench`. It reaches both hosts over SSH,
  installs the harness on them and starts the run. It doesn't take part in the measurements, and you
  can close it while a run goes on.
- **The client host** runs the tools' clients, and the agent that runs the experiment. The agent
  reaches the server over SSH, at the address in the inventory.
- **The server host** runs the tools' servers.

Each host has a data directory, such as `/bench`, on the disk being measured. The harness keeps
everything there: the corpus, the tools, the servers' stores and the results.

The client fetches the corpus and the tool builds from the URLs in the spec, checks each against its
SHA-256, and copies the server's builds to the server.

## Requirements

- **Operator's machine:** Python 3.11 or newer, `ssh`, `scp` and `tar`.
- **Hosts:** Ubuntu 24.04 or newer, or Debian 12 or newer. They need Python 3.11 or newer, and the
  bootstrap installs everything else with apt. You need root on each host once, to run the
  bootstrap.
- **Network:** the client must reach these ports on the server:

  | Port | Used by |
  |---|---|
  | SSH, the port in the inventory | the agent's commands to the server |
  | 1666/tcp | P4 |
  | 41337/udp and 41337/tcp | Lore |
  | 5202/tcp and 5202/udp | iperf3, for calibration, and the round-trip probes |

- **Disk:**
  - The client's data disk holds the corpus archive while it unpacks, the unpacked tree, and each
    tool's fetched copy, so plan for about four times the corpus size.
  - The server's holds each tool's store.

  For a 35 GB corpus, 600 GB on the client and 200 GB on the server are enough.

## Quick start

[QUICKSTART.md](QUICKSTART.md) walks through a first run. With two prepared hosts, a corpus archive
and the tool builds at URLs both can reach, a run is these commands, from this directory:

```sh
python3 -m bench validate my.toml     # check the spec and the inventory
python3 -m bench run my.toml          # start the run, detached on the client
python3 -m bench watch my.toml        # follow it until it ends
python3 -m bench collect my.toml      # copy the results to ~/.local/share/bench/results/<run-id>
python3 -m bench report ~/.local/share/bench/results/<run-id>    # write report.html there
```

## Step by step

### 1. Make the harness's SSH key

The harness uses its own key, in `$BENCH_STATE`, which is `~/.local/share/bench` by default.

```sh
mkdir -p ~/.local/share/bench
ssh-keygen -t ed25519 -N '' -C bench -f ~/.local/share/bench/bench_ed25519
```

Authorize `bench_ed25519.pub` for the benchmark user on both hosts. `bench run` copies the private
key to the client, which uses it to reach the server.

### 2. Prepare the hosts

Every host runs `host/bootstrap.sh USER DATA [DEV]` once, as root. It:

- installs the packages the harness uses (iperf3, sysstat, fio, zstd and a few more);
- gives USER the data directory DATA;
- installs `host/bench-helper` as USER's only root command;
- stops the timers that would add disk or CPU work during a run.

DEV is the network interface that reaches the other host, which the harness shapes and measures.
By default it's the interface of the default route; name it when the hosts reach each other over
another one.

`bench-helper` does only what a run needs as root: shaping the link, dropping caches, TRIM and a
fixed list of kernel settings. It checks every argument. Shaping also turns the interface's
segmentation offload (TSO and GSO) on, and leaves it on.

```sh
scp -r host/ USER@HOST:bench-host/
ssh -t USER@HOST sudo bench-host/bootstrap.sh USER /bench
```

Mount the disk to be measured on DATA first. XFS or ext4 with `noatime` work well. The harness TRIMs
the data volume before every test, so the disk should support it.

A run installs the harness on both hosts, but not `bench-helper`, which belongs to root. When
`host/bench-helper` changes, run the bootstrap on each host again. A run refuses a host whose
installed helper isn't the harness's copy, because that helper would shape the link differently from
what the results record.

[Local machines](#local-machines) and [Cloud instances](#cloud-instances) cover the details for each
kind of host. VMs made with `vm/` run the bootstrap themselves.

### 3. Put the corpus and the builds where the client can fetch them

The corpus is a tar archive: `.tar`, `.tar.gz`, `.tar.xz` or `.tar.zst`. Its `root` is the
directory inside the archive that the tools import.

```sh
tar -C /path/to/parent -cf - Engine | zstd -T0 -10 > corpus.tar.zst
sha256sum corpus.tar.zst
```

Each tool needs its binaries: `p4d` and `p4` for P4, and `lore` and `loreserver` for Lore. Each one
is pinned by its SHA-256.

The spec can name these URL forms:

| URL | Fetched with | Notes |
|---|---|---|
| `https://...` and `http://...` | curl | Resumes an interrupted download. A presigned object storage URL works too. |
| `file:///path` | nothing | The file must be on the client host. The file is used in place. |
| `s3://bucket/key` | the aws CLI | Needs the CLI and credentials on the client. |
| `gs://bucket/key` | gcloud | Needs the CLI and credentials on the client. |
| `gdrive://FILE_ID`, or a `drive.google.com` share link | curl | The name has no extension, so set `corpus.format`. Google Drive limits how often a large file can be downloaded, so prefer the other forms for large corpora. |

Once fetched, the corpus is unpacked once and kept. Later runs with the same archive reuse it.

### 4. Describe the hosts in an inventory

```toml
[client]
ip = "192.0.2.10"              # the address the server sees the client at
ssh = "bench@192.0.2.10:22"    # how the operator reaches it
jump = "you@gateway"           # optional: an SSH jump host for the operator
data = "/bench"
rated = { read_mbs = 7200, write_mbs = 6600 }   # optional: the data disk's rated speeds
notes = "what this host is"    # optional, kept with the results

[server]
ip = "192.0.2.20"              # the client reaches the server at this address and the SSH port above
ssh = "bench@192.0.2.20:22"
data = "/bench"
```

`rated` gives the report its baseline: how long copying the corpus at the disks' rated speeds takes.
For a drive with an SLC cache, add `cache_gb` and `after_cache_mbs`.

### 5. Write the spec

Start from `examples/workstation-vms.toml`, and see [The spec](#the-spec). Then check it:

```sh
python3 -m bench validate my.toml
```

`validate` prints the spec's identity and the number of runs, or every problem it found.

### 6. Run, watch, collect, report

`bench run` installs the harness on both hosts and starts the agent on the client, detached, so you
can disconnect. Follow the run with `bench watch` ([Watching a run](#watching-a-run)).

When the run ends, `bench collect` copies its results to `$BENCH_STATE/results/<run-id>`, and
`bench report` turns them into one HTML page.

To resume a run that stopped, give `bench run` its id: `--run-id <run-id>`. The resumed run skips
every completed run that had no failed step or verification, and repeats the rest. A run can only
be resumed with the spec it started from, and not while its agent is running. Resuming applies the
host settings again, in case a host restarted, and stops if either host is no longer the kind it
was. It also calibrates each profile again. Without `--run-id`, `bench run` starts a new run.
`status`, `watch` and `collect` follow the spec's latest run.

`bench prepare` does only the preparation, in the foreground: it fingerprints both hosts, applies
the settings, and fetches the tools and the corpus. It's useful for checking a new setup before a
long run.

## Local machines

### Bare metal

Run the bootstrap on each machine as in [step 2](#2-prepare-the-hosts), mount the data disk, and
list both machines in the inventory. If the machines are on one LAN, the unshaped round trip
between them is a fraction of a millisecond.

A profile's `rtt_ms` is the round trip the tools see. Before each profile the harness measures the
unshaped round trip, and the shaper on each host adds half of what it lacks of the profile's to what
that host sends. The harness then measures the shaped round trip and corrects the delay by what it
misses. The shaper also limits each host's send rate.

Set both machines' CPU frequency governor to `performance` for a run, as for
[VMs](#vms-with-vm): deep idle states make the round trip jump between measurements.

The link between the machines must be faster than the profile's rate. A shaped
5 Gbit/s profile needs a link faster than 5 Gbit/s, and `rate_mbit` must leave room for Ethernet
framing ([The spec](#the-spec)).

### VMs with vm/

`vm/vm.sh` runs a VM in a container built from `vm/Dockerfile`, so every host gets the same QEMU.
The container's network interface is bridged to the VM, which takes over the container's address.
Docker's NAT and published ports then reach the VM directly. Two workstations can each host one VM,
or one machine can host both.

Each VM host needs Docker, `/dev/kvm` and `/dev/vhost-net`, and an Ubuntu cloud image. On each host:

```sh
export VMDIR=/bench/vm                       # any directory on a fast disk
mkdir -p $VMDIR/kit
cp -r vm/Dockerfile vm/run-vm.sh vm/vm.sh host $VMDIR/kit/
curl -o $VMDIR/ubuntu-26.04-server-cloudimg-amd64.img \
    https://cloud-images.ubuntu.com/releases/26.04/release/ubuntu-26.04-server-cloudimg-amd64.img
cp ~/.local/share/bench/bench_ed25519.pub $VMDIR/bench.pub
docker build -t bench-vm:1 $VMDIR/kit        # or docker save here and docker load on the other host
```

Then start the VMs: 16 vCPUs, 32 GiB of memory, and data disks of 600 GB on the client and 200 GB on
the server. Here `HOSTIP` is the server workstation's address.

```sh
# on the client's host: SSH on its loopback; the operator jumps through this host
$VMDIR/kit/vm.sh up client 16 32 600 $VMDIR/bench.pub 127.0.0.1:2222:22

# on the server's host: SSH, P4, Lore and iperf3, published on the host's address
$VMDIR/kit/vm.sh up server 16 32 200 $VMDIR/bench.pub HOSTIP:2222:22 HOSTIP:1666:1666 \
    HOSTIP:41337:41337/udp HOSTIP:41337:41337/tcp HOSTIP:5202:5202/tcp HOSTIP:5202:5202/udp
```

`examples/workstation-vms.hosts.toml` is the inventory for this layout. Stop a VM with
`vm.sh down NAME`, and delete its disks with `vm.sh destroy NAME`.

The client VM can fetch the corpus and builds from its own host over Docker's bridge, which is
faster than any download:

```sh
python3 -m http.server 8080 --bind 172.17.0.1 --directory /path/to/www
```

The example spec's URLs point there (`http://172.17.0.1:8080/...`).

On a VM's first boot, cloud-init:

- creates the user `bench` with the key;
- formats the data disk as XFS on `/bench`;
- runs the bootstrap.

The VM's data disk is a raw image opened with direct I/O, so the host's page cache stays out of the
measurements. Its memory is allocated up front.

Two things on the VM hosts set the results as much as the spec does:

- **The CPU frequency governor.** Set it to `performance` on both hosts for a run, for example with
  `sudo cpupower frequency-set -g performance`. With `powersave`, a host's cores sleep deeply between
  packets, and the VM's network path then jumps by up to a millisecond between measurements, which
  no shaping can take out.
- **The drive holding the VM's disk.** The harness TRIMs the VM's data volume before every test, but
  that only frees blocks in the image file: the host's drive learns of them when the host trims its
  own filesystem. A nearly full drive, a QLC one especially, writes at a fraction of its rated speed.
  Keep it well short of full, and run `sudo fstrim -v` on the host's filesystem before a series of
  runs. The calibration's disk figures show when a drive has slowed.

Starting a VM again with different user data, such as a new kit or key, makes cloud-init treat it as
a new instance. It runs the bootstrap again and makes new SSH host keys, and SSH then refuses the
VM. Remove the VM's old entries with
`ssh-keygen -R '[127.0.0.1]:2222' -f ~/.local/share/bench/known_hosts`, and the same on the client
VM for the server, in `~/.ssh/bench_known_hosts`.

The guest's segmentation offload is limited to what the host's network card can offload. Without
that limit, a host with a Realtek 5 GbE card segments every guest packet in software and sends at
most 3.7 Gbit/s.

## Cloud instances

A cloud run uses the same static target as local machines: you start two instances, prepare them,
and list them in an inventory. The harness hasn't had a full run in the cloud yet, so check the
calibration of a first run closely. A later version will provision the instances itself.

On AWS, for example:

1. **Import the harness's key** as an EC2 key pair, so the image's default user (`ubuntu`) accepts
   it:

   ```sh
   aws ec2 import-key-pair --key-name bench \
       --public-key-material fileb://$HOME/.local/share/bench/bench_ed25519.pub
   ```

2. **Start two instances** from an Ubuntu 24.04 or newer image. Put them in one subnet and in a
   cluster placement group, so the unshaped round trip is small and steady.
   - Choose instance types whose *baseline* network bandwidth is above the profile's rate. Many
     types state an "up to" figure, which they sustain only in bursts.
   - Choose types with local NVMe (instance store) for the data disk, or give an EBS volume
     provisioned throughput: a gp3 volume's default of 125 MB/s would set the times on its own.

3. **Open the ports.** Give the server a security group that admits the client's private address on
   the ports in [Requirements](#requirements). Give both instances SSH access from the operator.

4. **Prepare each instance.** Format and mount the data disk, then run the bootstrap with `ubuntu`
   as the benchmark user. `lsblk` names the instance-store device, often `/dev/nvme1n1`.

   ```sh
   ssh ubuntu@INSTANCE 'sudo mkfs.xfs -f /dev/nvme1n1 && sudo mkdir -p /bench && sudo mount -o noatime /dev/nvme1n1 /bench'
   scp -r host/ ubuntu@INSTANCE:bench-host/
   ssh ubuntu@INSTANCE sudo bench-host/bootstrap.sh ubuntu /bench
   ```

   Instance store is erased when an instance stops. The mount doesn't survive a stop, and neither
   does an unpacked corpus.

5. **Write the inventory.** Use the private addresses for `ip` and the public addresses for `ssh`;
   `examples/cloud.hosts.toml` shows it. Serve the corpus and builds from object storage in the same
   region, with presigned HTTPS URLs; these need no credentials on the instances. An `s3://` URL
   works too, if the client has the aws CLI and an instance profile.

6. **Run** as in [step 6](#6-run-watch-collect-report). Terminate the instances when the results
   are collected.

On AWS, `wire` counters include the ENA driver's allowance counters. A result where the instance
exceeded its bandwidth or packet allowance is flagged `throttled`.

## The spec

A spec is TOML. `examples/workstation-vms.toml` is complete, and `bench/spec.py` checks every key.

| Section | Keys |
|---|---|
| top level | `name` (lowercase letters, digits and hyphens), `description` |
| `[corpus]` | `url`, `sha256`, `root` (the directory in the archive to import), `format` (`auto` from the name, or `tar`, `tar.gz`, `tar.xz`, `tar.zst`) |
| `[target]` | `kind = "static"`, `inventory` (a path relative to the spec) |
| `[host.sysctl]` | kernel settings applied to both hosts: TCP buffer sizes (`net.ipv4.tcp_rmem`, `tcp_wmem`), socket buffer limits (`net.core.rmem_max`, `wmem_max`, `rmem_default`, `wmem_default`), `net.core.netdev_max_backlog`, congestion control, the default qdisc, the `vm.dirty_*` settings and `vm.swappiness`. `"inherit"` leaves a setting as the host has it. |
| `[[profile]]` | `name`, `rtt_ms` (the round trip the tools see, the unshaped link's included), `rate_mbit`, `queue_bdp` (the shaper's queue in bandwidth-delay products, default 1), `loss_ppm`, `jitter_ms` |
| `[tool.p4]` | `p4d` and `p4` (each `{ url, sha256 }`), `port` (`tcp` or `ssl`), `typemap` (`none`, `ue` or a file), `submit_threads`, `sync_threads`, `configurables` (set with `p4 configure`) |
| `[tool.lore]` | `lore` and `loreserver` (each `{ url, sha256 }`), `client_env` (environment for the client), `server_config` (a TOML file for the server) |
| `[run]` | `tests` (`import`, `fetch`), `repetitions`, `order` (`alternate` or `fixed`), `settle_s` (the pause before each test), `verify_sha256` (`first`, `all` or `none`), `stall_s` and `step_timeout_s` ([Watching a run](#watching-a-run)) |
| `[run.calibration]` | `rtt_tolerance_ms`, `rtt_tolerance_rel`, `tcp_min_fraction`, `disk` (measure the disks), `enforce` (stop a profile whose link is out of tolerance) |

The shaper counts Ethernet frames without the 24 bytes of preamble, gap and checksum each takes on
the wire. A `rate_mbit` of 4920 is therefore 5 Gbit/s on the wire.

A spec's identity is the hash of its content and of every file it refers to, so reformatting it
changes nothing, and editing a referenced file changes the identity. A result's series key covers
everything that affects its numbers except the tool's build: the method, the corpus, the profile,
the test, the tool's settings, the host settings and the hosts' hardware class.

## What a run does

**Preparing.** The agent applies the spec's kernel settings and fingerprints both hosts: CPU,
memory, kernel, network card, data disk and settings. It then measures the unshaped round trip, and
fetches the tools and the corpus.

**For each profile.** The agent shapes both hosts and calibrates the link:

- The agent measures the unshaped round trip again, since a VM's network path adds tenths of a
  millisecond to it and how much drifts over a run of hours.
- Shaping applies to traffic to the other host only: netem with half of what the unshaped round
  trip lacks of the profile's in each direction, then tbf at the rate, with a queue of one
  bandwidth-delay product. Other traffic goes out unshaped.
- The shaped link is slower than the unshaped round trip plus the delay, since the shaper takes
  time of its own and the unshaped round trip varies between measurements. The agent measures the
  shaped round trip and corrects the delay by what it misses, at most twice. The calibration
  records the unshaped round trip, the shaped one and the delay that stood.
- Calibration measures the round trip idle and under load, TCP and UDP throughput in both
  directions, and both disks. The round trip is the time a TCP connection to the server's iperf3
  takes to open, not a ping, which a VM's host answers itself, and not sshd, which refuses a source
  that opens many connections without logging in.

What the calibration's figures mean:

- **TCP** runs flat out, so it measures what the link carries: about 941 Mbit/s for a 1 Gbit/s
  profile, the rate less the headers.
- **UDP** has no congestion control, so the check offers 92% of the rate, a little below the
  shaper's limit, and reports what arrived and what was lost. A healthy link delivers all of it. At
  several Gbit/s, iperf3 in a VM can run out of CPU before it sends all it was asked to: a figure
  below the offered rate with no loss is the sender's, not the link's.
- **Disks** are measured with 4 GB of sequential direct I/O and a write of 8,192 files of 64 KB.
  The small-file figure varies by up to a third between back-to-back checks, so compare several
  before reading much into one. Compare the sequential figures with earlier runs on the same hosts
  before trusting a run's times.

If the link is out of tolerance, the agent skips the profile and goes on with the next. The run then
ends with exit code 1.

**For each run.** The agent restores the corpus tree and resets the server to an empty store. Then,
for each test, it:

1. TRIMs both data volumes, lets them settle, and drops both page caches;
2. runs the tool's steps under GNU time, recording system activity on both hosts every second;
3. times separately the flush of what the tool left in memory.

A fetch is verified against the corpus: the file list and every file's size every time, and every
file's SHA-256 on the first repetition (`verify_sha256`). A step's wall clock is the agent's own;
GNU time gives its CPU time and peak memory.

**Flags** mark results that may not mean what they seem. A flagged result is kept and shown, and
left out of trend medians.

| Flag | Meaning |
|---|---|
| `step-failed` | A step exited with an error. |
| `stalled` | The watchdog stopped a step. |
| `verify-failed` | The fetched tree differs from the corpus. |
| `steal` | The hypervisor took CPU time from a VM. |
| `nic-drops` | The network card had no room for received frames, or the kernel's receive backlog dropped them. |
| `throttled` | An AWS instance exceeded a network allowance. |
| `busy-server` | Something other than the tool used the server's CPU. |

## Watching a run

A long run must not sit in a hang. Two mechanisms guard against one.

**The agent's watchdog.** While a step runs, the agent checks every 10 seconds whether the step
moves data over the network or the disk at more than 100 kB/s, or uses CPU itself. A step with no progress for `run.stall_s`
(10 minutes by default) is stopped and flagged `stalled`. So is a step that runs longer than
`run.step_timeout_s` (6 hours by default). Stopping a step stops every process in its session,
which is every process it starts unless one starts a session of its own. Every
command to a host has a timeout too, 15 minutes unless the operation needs longer, and so do
downloads that stop moving.

**`bench watch`** follows a run from the operator's machine. It prints:

- each new log line;
- a progress line every 5 minutes, with the step's network rate and CPU;
- a `STALL?` warning when a step has made no progress for 3 minutes (`--stall`), when a step's
  heartbeat is older than 5 minutes, or when no log line has come for 10 minutes outside a step.

It exits when the agent stops, as the agent recorded it in `end.json`: 0 when every run is done, 1
when the agent stopped early or was killed. If it can't reach the client 10 times in a row, it gives
up and exits 2.

```sh
python3 -m bench watch my.toml --stall 180 --every 300
```

`bench status` prints the end of the log once, and says whether the agent is running.

## Results and reports

A run's results stay on the client, in `<data>/runs/<run-id>`:

```
spec.json            the resolved spec, with the inventory and the harness's hash
run.log              what the agent did, with times
heartbeat.json       the agent's latest heartbeat during a step, for watch
end.json             how the agent ended: its exit code, and the error if there was one
prepare/             fingerprints, settings, tools and the corpus description
<profile>/calibration.json
<profile>/<tool>-r<N>/result.json   one run: every step, the counters, the flags, the verification
<profile>/<tool>-r<N>/...           the raw files behind it: system activity, logs, timings
```

`bench collect` copies the directory to the operator's machine. `bench report RUN_DIR` writes one
HTML page with the calibration, a chart per test against the disks' baseline, and per-test tables:
times, bytes on the wire, CPU, memory, disk activity, TCP retransmits, UDP receive-buffer drops,
flags and verification. It also plots the client's network rate over each test. A run that failed,
stalled or didn't verify is listed with its flags, and left out of the charts and figures.

`bench report --trend RUN_DIR...` compares runs of the same series across tool builds, with one
table and chart per series and a row per build. A build is told apart by its binaries' SHA-256,
since builds without a release stamp share a version. Medians leave out flagged runs.

## Tests

```sh
python3 -m pytest -q -o addopts= -p no:cacheprovider -c /dev/null tests
```

The tests cover:

- the spec and inventory checks;
- `bench-helper`'s argument checking and shaping, through its dry-run mode, and the check that a
  host's installed helper is the harness's;
- the flags and the counters;
- verification, restoring the corpus, and the corpus cache;
- resuming a run, one agent at a time, and how an agent ends;
- the watchdog, including a step whose processes ignore SIGTERM;
- the trend report.

## Limitations

- Targets are static: the harness doesn't start hosts itself yet. The spec's `aws` and `helm`
  target kinds are reserved for that.
- There is one client and one server, reached over IPv4, and the roles can't be swapped within a
  spec yet.
- Adapters exist for P4 and Lore.
- Hosts must be Debian or Ubuntu, because the bootstrap uses apt.
- One network interface per host is shaped and measured: the one the bootstrap recorded.
- On VMs, TRIM reaches the VM's disk image but not the host's drive, which the host has to trim
  itself ([VMs with vm/](#vms-with-vm)).
