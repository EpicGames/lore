#!/bin/bash
# vm.sh: start, stop or delete a benchmark VM on this host. Runs as an ordinary user who may use
# Docker; the VM runs in the bench-vm image (Dockerfile beside this script).
#
#   vm.sh up NAME CPUS MEM_GIB DATA_GIB PUBKEY_FILE [PUBLISH]...
#       PUBLISH is a docker --publish value, such as 127.0.0.1:2222:22 or 10.0.0.5:41337:41337/udp.
#   vm.sh down NAME       power the VM off and remove its container; the disks stay
#   vm.sh destroy NAME    the same, and delete the VM's disks
#
# VMDIR (environment) holds the cloud image named by VM_IMAGE, this kit in VMDIR/kit, and one
# directory per VM. The guest's first boot creates the user bench with PUBKEY_FILE as its key,
# formats the data disk as XFS on /bench and runs host/bootstrap.sh.
set -euo pipefail
: "${VMDIR:?set VMDIR}" "${VM_IMAGE:=ubuntu-26.04-server-cloudimg-amd64.img}" "${VM_TAG:=bench-vm:1}"
kit=$VMDIR/kit
cmd=${1:-}; name=${2:-}
[ -n "$name" ] || { sed -n '2,12p' "$0" >&2; exit 2; }
container=bench-vm-$name

b64() { base64 -w0 < "$1"; }

case $cmd in
up)
    [ $# -ge 6 ] || { sed -n '2,12p' "$0" >&2; exit 2; }
    cpus=$3 mem=$4 data=$5 key=$(cat "$6"); shift 6
    mkdir -p "$VMDIR/$name"
    # The guest must not build segmentation-offload packets larger than the host's card can
    # offload: the host would then segment each one in software and send it frame by frame, which
    # caps throughput at the card's packet rate (3.7 Gbit/s on a Realtek 5 GbE card).
    nic=$(ip -4 route show default | awk '{print $5; exit}')
    gso=$(ip -d link show "$nic" | grep -oE 'gso_max_size [0-9]+' | head -1 | cut -d' ' -f2) || true
    gso_unit=$(printf '[Unit]\nDescription=Limit segmentation offload to what the host card offloads\nAfter=systemd-networkd.service\n[Service]\nType=oneshot\nExecStart=/usr/sbin/ip link set dev eth0 gso_max_size %s\n[Install]\nWantedBy=multi-user.target\n' "${gso:-65536}" | base64 -w0)
    cat > "$VMDIR/$name/user-data" <<EOF
#cloud-config
hostname: $name
users:
  - name: bench
    shell: /bin/bash
    lock_passwd: true
    ssh_authorized_keys: ["$key"]
fs_setup:
  - {label: benchdata, filesystem: xfs, device: /dev/disk/by-id/virtio-benchdata, overwrite: false}
mounts:
  - [LABEL=benchdata, /bench, xfs, "defaults,noatime", "0", "2"]
write_files:
  - {path: /root/bench-host/bootstrap.sh, permissions: "0755", encoding: b64, content: $(b64 "$kit/host/bootstrap.sh")}
  - {path: /root/bench-host/bench-helper, permissions: "0755", encoding: b64, content: $(b64 "$kit/host/bench-helper")}
  - {path: /etc/systemd/system/bench-gso.service, permissions: "0644", encoding: b64, content: $gso_unit}
runcmd:
  - [systemctl, enable, --now, bench-gso.service]
  - [bash, /root/bench-host/bootstrap.sh, bench, /bench]
EOF
    publish=()
    for p in "$@"; do publish+=(--publish "$p"); done
    docker rm -f "$container" > /dev/null 2>&1 || true
    docker run -d --name "$container" --stop-timeout 180 \
        --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN \
        -e VM_NAME="$name" -e VM_CPUS="$cpus" -e VM_MEM_GIB="$mem" -e VM_DATA_GIB="$data" -e VM_IMAGE="$VM_IMAGE" \
        -v "$VMDIR:/vm" "${publish[@]}" "$VM_TAG"
    docker inspect --format '{{.Name}} {{.State.Status}} {{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}} image {{.Image}}' "$container"
    ;;
down|destroy)
    docker stop "$container" > /dev/null 2>&1 || true
    docker rm -f "$container" > /dev/null 2>&1 || true
    if [ "$cmd" = destroy ]; then
        rm -f "$VMDIR/$name/os.qcow2" "$VMDIR/$name/data.raw" "$VMDIR/$name/seed.iso"
    fi
    ;;
*)
    sed -n '2,12p' "$0" >&2; exit 2 ;;
esac
