# bench quick start

This page takes you from nothing to a first report. There are three ways to get the two hosts a run
needs: two Linux machines, two VMs on two workstations, or two cloud instances. The
[README](README.md) explains every step in full.

A run uses three machines. You run `python3 -m bench` on your own machine, and it reaches two hosts,
a client and a server, over SSH. Run every command here on your machine, from this directory,
unless the step says otherwise.

1. [What you need](#1-what-you-need)
2. [Make the key, the corpus and the builds](#2-make-the-key-the-corpus-and-the-builds)
3. Get two hosts ready, one of:
   [A. Two Linux machines](#a-two-linux-machines),
   [B. Two VMs on two workstations](#b-two-vms-on-two-workstations),
   [C. Two cloud instances](#c-two-cloud-instances)
4. [Write the spec](#4-write-the-spec)
5. [Run it](#5-run-it)
6. [Compare builds](#6-compare-builds)
7. [When something goes wrong](#7-when-something-goes-wrong)

## 1. What you need

- **Your machine:** Python 3.11 or newer, `ssh`, `scp`, `tar` and `zstd`, and a Rust toolchain to
  build Lore.
- **Two hosts:** Ubuntu 24.04 or newer, or Debian 12 or newer, where you can use sudo once. The
  client must reach the server on the ports listed in [Requirements](README.md#requirements).
- **A tree to import.** For a first run, a directory of a few GB is enough. The client's disk needs
  about four times the tree's size.

## 2. Make the key, the corpus and the builds

The harness uses its own SSH key. Everything the client fetches goes in one directory,
`~/bench-www`, which step 3 serves to it, and your spec and inventory go in `~/bench-specs`.

```sh
mkdir -p ~/.local/share/bench ~/bench-www ~/bench-specs
ssh-keygen -t ed25519 -N '' -C bench -f ~/.local/share/bench/bench_ed25519

# The corpus: a tar archive of the tree. The tree's directory name is the spec's corpus.root.
tar -C /path/to/parent -cf - mytree | zstd -T0 -10 > ~/bench-www/corpus.tar.zst

# Lore, with the profile releases use
(cd ../.. && cargo build --profile release-lto --bin lore --bin loreserver)
cp ../../target/release-lto/lore ../../target/release-lto/loreserver ~/bench-www/

# P4, if the run includes it: p4d and p4 for Linux from Perforce (r25.1 here, or a newer release)
curl -o ~/bench-www/p4d https://filehost.perforce.com/perforce/r25.1/bin.linux26x86_64/p4d
curl -o ~/bench-www/p4 https://filehost.perforce.com/perforce/r25.1/bin.linux26x86_64/p4

sha256sum ~/bench-www/*    # the spec pins every file by these hashes
```

## 3. Get two hosts ready

Pick one of the three. Each ends with an inventory, `~/bench-specs/my.hosts.toml`, and a URL the
client fetches the files from.

### A. Two Linux machines

Two machines on one network. The link between them must be faster than the profile's rate: a
1 Gbit/s network takes a profile of up to 900 Mbit/s.

Mount the disk you want to measure on `/bench` on each machine. For a first run, a directory on an
existing SSD's filesystem works, as long as it supports TRIM. For results you'll compare, give the
data a disk of its own. The benchmark user can be your own account, here `you`:

```sh
for host in you@192.0.2.10 you@192.0.2.20; do
    ssh-copy-id -i ~/.local/share/bench/bench_ed25519.pub $host
    scp -r host/ $host:bench-host/
    ssh -t $host sudo bench-host/bootstrap.sh you /bench
done
```

For results you'll compare, set both machines' CPU governor to `performance` before a run
(`sudo cpupower frequency-set -g performance`): deep idle states make the round trip jump.

Serve the files from your machine, and leave the server running until the run has fetched them:

```sh
python3 -m http.server 8080 --directory ~/bench-www
```

The spec's URLs are then `http://YOUR_MACHINE:8080/<file>`. Write `~/bench-specs/my.hosts.toml`:

```toml
[client]
ip = "192.0.2.10"            # the client's address, as the server sees it
ssh = "you@192.0.2.10:22"    # how your machine reaches it
data = "/bench"

[server]
ip = "192.0.2.20"            # the client reaches the server at this address
ssh = "you@192.0.2.20:22"
data = "/bench"
```

### B. Two VMs on two workstations

One VM on each of two workstations, each in a container that gives it the same QEMU. This is the
layout the example spec is written for. Each workstation needs Docker, `/dev/kvm` and
`/dev/vhost-net`.

On each workstation, from a copy of this directory with the harness's public key copied into it:

```sh
export VMDIR=/bench/vm    # any directory on a fast disk
mkdir -p $VMDIR/kit
cp -r vm/Dockerfile vm/run-vm.sh vm/vm.sh host $VMDIR/kit/
cp bench_ed25519.pub $VMDIR/bench.pub
curl -o $VMDIR/ubuntu-26.04-server-cloudimg-amd64.img \
    https://cloud-images.ubuntu.com/releases/26.04/release/ubuntu-26.04-server-cloudimg-amd64.img
docker build -t bench-vm:1 $VMDIR/kit
```

Start the client VM on one workstation, and the server VM on the other. The numbers are vCPUs,
memory in GiB and the data disk in GB. `SERVER_HOST` is the server workstation's address.

```sh
# On the client workstation: the VM's SSH on the workstation's loopback
$VMDIR/kit/vm.sh up client 16 32 600 $VMDIR/bench.pub 127.0.0.1:2222:22

# On the server workstation: SSH, P4, Lore and iperf3 on the workstation's address
$VMDIR/kit/vm.sh up server 16 32 200 $VMDIR/bench.pub SERVER_HOST:2222:22 SERVER_HOST:1666:1666 \
    SERVER_HOST:41337:41337/udp SERVER_HOST:41337:41337/tcp \
    SERVER_HOST:5202:5202/tcp SERVER_HOST:5202:5202/udp
```

On its first boot, each VM formats its data disk on `/bench` and runs the bootstrap, which takes a
few minutes.

Before a series of runs, on each workstation:

```sh
sudo cpupower frequency-set -g performance   # deep idle states make the VMs' round trip jump
sudo fstrim -v /bench                        # the filesystem holding $VMDIR: frees what the VMs trimmed
```

A VM's TRIM only frees blocks in its disk image; the drive learns of them when the host trims. A
nearly full drive writes at a fraction of its speed.

Serve the files from the client workstation, on its Docker bridge, where the client VM fetches them
faster than from anywhere else. Copy `~/bench-www` there first if your machine is another one.

```sh
python3 -m http.server 8080 --bind 172.17.0.1 --directory ~/bench-www
```

The spec's URLs are then `http://172.17.0.1:8080/<file>`, as in the example spec. Copy
`examples/workstation-vms.hosts.toml` to `~/bench-specs/my.hosts.toml`, and in it:

- replace `192.0.2.10` with the client workstation's address, and `192.0.2.20` with `SERVER_HOST`;
- replace `you` with your account on the client workstation, which your machine jumps through;
- replace the `rated` disk speeds and the `notes` with your own, or delete them.

### C. Two cloud instances

The harness hasn't had a full run in the cloud yet, so check the calibration of a first run
closely. The commands below are for AWS, with the aws CLI on your machine; `VPC_ID` and
`SUBNET_ID` are yours, and the instance type is an example.

Choose an instance type whose *baseline* network bandwidth is above the profile's rate, since many
types sustain their "up to" figure only in bursts, and one with local NVMe (instance store) for the
data. A c6id.8xlarge has 12.5 Gbit/s and a local NVMe drive.

```sh
KEY=~/.local/share/bench/bench_ed25519

# The harness's key as the instances' key pair, and a placement group that keeps them close
aws ec2 import-key-pair --key-name bench --public-key-material fileb://$KEY.pub
aws ec2 create-placement-group --group-name bench --strategy cluster

# The instances reach each other on every port, and your machine reaches them over SSH
SG=$(aws ec2 create-security-group --group-name bench --description bench --vpc-id VPC_ID \
    --query GroupId --output text)
aws ec2 authorize-security-group-ingress --group-id $SG --protocol -1 --source-group $SG
aws ec2 authorize-security-group-ingress --group-id $SG --protocol tcp --port 22 \
    --cidr $(curl -s https://checkip.amazonaws.com)/32

# Two instances from the current Ubuntu 24.04 image
AMI=$(aws ssm get-parameter --query Parameter.Value --output text \
    --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id)
aws ec2 run-instances --image-id $AMI --instance-type c6id.8xlarge --count 2 --key-name bench \
    --subnet-id SUBNET_ID --security-group-ids $SG --placement GroupName=bench \
    --associate-public-ip-address --query 'Instances[].InstanceId' --output text
aws ec2 describe-instances --instance-ids ID1 ID2 --output text \
    --query 'Reservations[].Instances[].[InstanceId,PrivateIpAddress,PublicIpAddress]'
```

Prepare each instance. `lsblk` names its instance-store drive, often `/dev/nvme1n1`, which is the
drive with no partitions on it:

```sh
for host in ubuntu@PUBLIC_IP1 ubuntu@PUBLIC_IP2; do
    ssh -i $KEY $host 'sudo apt-get install -y -q xfsprogs && sudo mkfs.xfs -f /dev/nvme1n1 &&
        sudo mkdir -p /bench && sudo mount -o noatime /dev/nvme1n1 /bench'
    scp -i $KEY -r host/ $host:bench-host/
    ssh -i $KEY $host sudo bench-host/bootstrap.sh ubuntu /bench
done
```

An instance store is erased when its instance stops, so prepare an instance again after a stop.

Put the files in an S3 bucket in the same region, and give the spec presigned URLs, which need no
credentials on the instances. A URL lasts at most 7 days, and one signed with temporary credentials
expires when they do.

```sh
aws s3 cp --recursive ~/bench-www s3://BUCKET/bench/
for f in ~/bench-www/*; do aws s3 presign s3://BUCKET/bench/${f##*/} --expires-in 604800; done
```

Copy `examples/cloud.hosts.toml` to `~/bench-specs/my.hosts.toml`, with each instance's private
address as `ip` and its public address in `ssh`. When the results are collected, terminate the
instances with `aws ec2 terminate-instances --instance-ids ID1 ID2`.

## 4. Write the spec

Start from the example:

```sh
cp examples/workstation-vms.toml ~/bench-specs/my.toml
```

Then change it:

| Key | Set it to |
|---|---|
| `name` | a name of your own, such as `first-run` |
| `[corpus]` `url`, `sha256`, `root` | the archive's URL and hash, and the tree's directory name in it |
| `[target]` `inventory` | `"my.hosts.toml"`, relative to the spec |
| `[[profile]]` `rtt_ms`, `rate_mbit` | the link to emulate. `rtt_ms` is the round trip the tools see: the harness measures the unshaped one and adds the rest. `rate_mbit` must be below the real link's speed: `900` on a 1 Gbit/s network. `4920` is 5 Gbit/s on the wire. |
| `[tool.lore]` and `[tool.p4]` `url`, `sha256` | the builds' URLs and hashes |
| `[run]` `settle_s` | `10` for a first run. The default, 120 seconds before each test, lets an SSD settle after TRIM. |

To run Lore alone, delete the `[tool.p4]` and `[tool.p4.configurables]` tables. Then check the spec:

```sh
python3 -m bench validate ~/bench-specs/my.toml
```

It prints the spec's identity and the number of runs, or every problem it found.

## 5. Run it

```sh
python3 -m bench prepare ~/bench-specs/my.toml   # optional: check the hosts, fetch the tools and corpus
python3 -m bench run ~/bench-specs/my.toml       # start the run on the client, detached
python3 -m bench watch ~/bench-specs/my.toml     # follow it until it ends
dir=$(python3 -m bench collect ~/bench-specs/my.toml)
python3 -m bench report "$dir"                   # writes $dir/report.html
```

`prepare` keeps what it fetched, so `run` doesn't fetch it again. Once the run has started, you can
close your machine, and `watch` again later. `watch` exits with 0 when every run is done.

Open `report.html` in a browser. It shows how the link calibrated, then each test's times against
the disks' baseline, with the bytes on the wire, CPU, memory and disk activity, and whether every
fetched tree matched the corpus.

## 6. Compare builds

Results from the same spec on the same kind of hosts form a series. To compare a new build of a tool
with an earlier one, change only that tool's `url` and `sha256`, run again, collect, and give both
results to the report:

```sh
python3 -m bench report --trend "$before" "$after"   # writes trend.html in the first directory
```

## 7. When something goes wrong

| What you see | What to do |
|---|---|
| `validate` prints spec errors | Each error names its key. [The spec](README.md#the-spec) lists them all. |
| SSH refuses a VM's host key after the VM started again | Starting it with new user data made new host keys. Remove the client VM's old entry with `ssh-keygen -R '[127.0.0.1]:2222' -f ~/.local/share/bench/known_hosts`. For the server VM, do the same on the client VM, in `~/.ssh/bench_known_hosts`. |
| A run refuses a host's `bench-helper` | `host/bench-helper` changed since the bootstrap. Run the bootstrap on that host again. |
| The log says a profile's link is out of tolerance, and the run skips it | The real link can't carry `rate_mbit`, or its round trip isn't steady. Lower `rate_mbit`, or see [What a run does](README.md#what-a-run-does). |
| `watch` prints `STALL?` | A step has made no progress for 3 minutes. The agent stops a step after 10 minutes without progress, flags it `stalled`, and goes on with the next run. |
| A run stopped part way | `python3 -m bench run ~/bench-specs/my.toml --run-id RUN_ID` resumes it. It skips every run that completed without a failed step or verification. |
| A disk's figure in the calibration is far below earlier runs | On VMs, the drive holding the disk image may be nearly full or untrimmed: trim the host's filesystem and check how full it is. The small-file figure alone varies by up to a third from one check to the next. |
| The idle round trip differs between profiles by more than a few tenths of a millisecond | Set the hosts' CPU governor to `performance`. |
| UDP arrives below the offered rate with no loss | The sender ran short of CPU, which happens in VMs at several Gbit/s. The link is fine. |

[Watching a run](README.md#watching-a-run) and [Results and reports](README.md#results-and-reports)
in the README go further.
