#!/bin/bash
# run-vm: the container's entry point. Boots one VM and bridges the container's network interface to
# it. The VM takes over the container's address and MAC, so Docker's NAT and published ports reach
# the VM directly, with no second NAT inside the container.
#
# Environment: VM_NAME, VM_CPUS, VM_MEM_GIB, VM_DATA_GIB, VM_IMAGE (a cloud image in /vm).
# /vm/$VM_NAME/user-data is the cloud-init user data; this script adds the network configuration,
# the metadata and the seed image, an OS disk backed by the image, and a raw data disk.
set -euo pipefail
name=${VM_NAME:?} cpus=${VM_CPUS:?} mem=${VM_MEM_GIB:?} data_gib=${VM_DATA_GIB:?} image=/vm/${VM_IMAGE:?}
dir=/vm/$name
[ -f "$dir/user-data" ] || { echo "run-vm: $dir/user-data is missing" >&2; exit 1; }

# The container's address, route, DNS servers and MAC, before the bridge takes the interface.
addr=$(ip -4 -o addr show dev eth0 | awk '{print $4}')
gw=$(ip -4 route show default | awk '{print $3}')
mac=$(cat /sys/class/net/eth0/address)
dns=$(awk '/^nameserver/ {printf "%s%s", sep, $2; sep=", "}' /etc/resolv.conf)

cat > "$dir/network-config" <<EOF
version: 2
ethernets:
  nic0:
    match: {macaddress: "$mac"}
    set-name: eth0
    addresses: [$addr]
    routes: [{to: default, via: $gw}]
    nameservers: {addresses: [$dns]}
EOF
printf 'instance-id: %s\nlocal-hostname: %s\n' "$name-$(md5sum < "$dir/user-data" | cut -c1-8)" "$name" > "$dir/meta-data"
rm -f "$dir/seed.iso"
xorriso -as mkisofs -quiet -o "$dir/seed.iso" -V cidata -J -r \
    "$dir/user-data" "$dir/meta-data" "$dir/network-config"

[ -f "$dir/os.qcow2" ] || qemu-img create -q -f qcow2 -F qcow2 -b "$image" "$dir/os.qcow2" 20G
[ -f "$dir/data.raw" ] || truncate -s "${data_gib}G" "$dir/data.raw"

# Hand the interface to a bridge with the VM's tap device. eth0 gets a new MAC so the VM can use
# the old one, which the host's neighbour table already maps to this address.
ip link set eth0 down
ip addr flush dev eth0
ip link set eth0 address "$(printf '02:%02x:%02x:%02x:%02x:%02x' $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)))"
ip link add br0 type bridge
ip link set eth0 master br0
ip tuntap add dev tap0 mode tap multi_queue
ip link set tap0 master br0
ip link set eth0 up
ip link set tap0 up
ip link set br0 up

queues=$(( cpus < 8 ? cpus : 8 ))
qemu-system-x86_64 \
    -name "$name" -machine q35,accel=kvm -cpu host \
    -smp "$cpus,sockets=1,cores=$cpus,threads=1" \
    -object "memory-backend-ram,id=mem0,size=${mem}G,prealloc=on" -machine memory-backend=mem0 -m "${mem}G" \
    -display none -monitor none -serial "file:$dir/console.log" \
    -qmp "unix:$dir/qmp.sock,server=on,wait=off" \
    -object iothread,id=io0 \
    -drive "file=$dir/os.qcow2,if=none,id=os,format=qcow2,cache=none,aio=native,discard=unmap" \
    -device virtio-blk-pci,drive=os,bootindex=0 \
    -drive "file=$dir/data.raw,if=none,id=data,format=raw,cache=none,aio=native,discard=unmap,detect-zeroes=unmap" \
    -device "virtio-blk-pci,drive=data,iothread=io0,num-queues=$queues,serial=benchdata" \
    -drive "file=$dir/seed.iso,if=none,id=seed,format=raw,readonly=on" \
    -device virtio-blk-pci,drive=seed \
    -netdev "tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on,queues=$queues" \
    -device "virtio-net-pci,netdev=n0,mac=$mac,mq=on,vectors=$((2 * queues + 2)),romfile=" &
qemu=$!

# docker stop: ask the guest to power off and wait for it, rather than pulling the plug.
powerdown() {
    printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"system_powerdown"}' \
        | nc -NU "$dir/qmp.sock" > /dev/null 2>&1 || kill "$qemu"
}
trap powerdown TERM INT
while kill -0 "$qemu" 2> /dev/null; do wait "$qemu" || true; done
