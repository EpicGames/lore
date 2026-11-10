# A container that boots one benchmark VM with QEMU and KVM. Building it once and loading the same
# image on every host gives every VM the same QEMU, whatever the hosts have installed.
FROM ubuntu:26.04
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        qemu-system-x86 qemu-utils seabios iproute2 xorriso netcat-openbsd ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY run-vm.sh /usr/local/bin/run-vm
ENTRYPOINT ["/usr/local/bin/run-vm"]
