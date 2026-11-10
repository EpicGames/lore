#!/bin/bash
# bootstrap.sh USER DATA [DEV]: prepare a benchmark host. Runs as root, once per host: cloud-init runs
# it on a VM's first boot, and on any other host an operator runs it with sudo. Every target runs this
# same script, so every host is prepared the same way.
#
# It installs the packages the harness uses, gives USER the data directory DATA, installs
# bench-helper (beside this script) as USER's only root command, and stops the timers that would
# add disk or CPU work during a run. DEV is the network interface that reaches the other host, the
# one the harness shapes and measures; by default, the interface of the default route.
set -euo pipefail
user=${1:?usage: bootstrap.sh USER DATA [DEV]} data=${2:?usage: bootstrap.sh USER DATA [DEV]}
here=$(cd "$(dirname "$0")" && pwd)

export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q --no-install-recommends \
    iperf3 sysstat fio zstd time ethtool curl python3 openssl xfsprogs iproute2 ca-certificates

id "$user" > /dev/null 2>&1 || useradd -m -s /bin/bash "$user"
mkdir -p "$data"
chown "$user:" "$data"

install -d /etc/bench
dev=${3:-$(ip -4 route show default | awk '{print $5; exit}')}
[ -d "/sys/class/net/$dev" ] || { echo "bootstrap.sh: no network interface ${dev:-(none)}" >&2; exit 1; }
printf 'DEV=%s\nDATA=%s\n' "$dev" "$data" > /etc/bench/helper.conf
install -m 0755 "$here/bench-helper" /usr/local/sbin/bench-helper
# Checked before it goes in place: a broken file in sudoers.d breaks sudo for everyone. sudo reads
# no file in sudoers.d whose name holds a dot, so the draft beside it is inert, and the rename
# puts it in place at once.
rule=/etc/sudoers.d/bench-helper.new
printf '%s ALL=(root) NOPASSWD: /usr/local/sbin/bench-helper\n' "$user" > "$rule"
chmod 0440 "$rule"
visudo -cqf "$rule" || { rm -f "$rule"; exit 1; }
mv "$rule" /etc/sudoers.d/bench-helper

for unit in sysstat.service sysstat-collect.timer sysstat-summary.timer unattended-upgrades.service \
            apt-daily.timer apt-daily-upgrade.timer fstrim.timer man-db.timer motd-news.timer; do
    systemctl disable --now "$unit" > /dev/null 2>&1 || true
done
date -u +%FT%TZ > /etc/bench/bootstrapped
