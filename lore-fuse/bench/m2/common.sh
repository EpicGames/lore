# shellcheck shell=bash
# Shared helpers for the milestone-2 build benchmarks. Sourced, not run.
#
# Environment (see README "Build benchmarks"):
#   M2_ENV     script exporting LORE, LORE_GLOBAL_PATH, LORE_SERVICE_SOCKET, LORE_SERVICE_EXECUTABLE,
#              LINUX_MULTIARCH_ROOT and BENCH_REV (default /tmp/lore-fuse-m2/env.sh)
#   M2_OUT     directory for run output (default /tmp/lore-fuse-m2/runs)
#   M2_CORES   CPU list the build is pinned to (default 0-11, the P-cores)
#   M2_ACTIONS UBT -MaxParallelActions (default 12)

M2_ENV=${M2_ENV:-/tmp/lore-fuse-m2/env.sh}
M2_OUT=${M2_OUT:-/tmp/lore-fuse-m2/runs}
M2_CORES=${M2_CORES:-0-11}
# shellcheck source=/dev/null
source "$M2_ENV"

# Processes that are never part of a measured build: other engineers' builds and tests.
M2_FOREIGN='cargo|rustc|clippy-driver|pytest|loreserver|lore-server'

# Busy CPU percentage over 3 s, from /proc/stat.
m2_cpu_busy_pct() {
    read -r _ u1 n1 s1 i1 w1 q1 sq1 st1 _ < /proc/stat
    sleep 3
    read -r _ u2 n2 s2 i2 w2 q2 sq2 st2 _ < /proc/stat
    local busy=$(( (u2+n2+s2+q2+sq2+st2) - (u1+n1+s1+q1+sq1+st1) ))
    local idle=$(( (i2+w2) - (i1+w1) ))
    echo $(( 100 * busy / (busy + idle) ))
}

# Blocks until three consecutive checks see no foreign process and under 5% CPU busy.
m2_wait_quiet() {
    local log=$1 quiet=0
    while [ $quiet -lt 3 ]; do
        if pgrep -x "$M2_FOREIGN" >/dev/null || [ "$(m2_cpu_busy_pct)" -ge 5 ]; then
            quiet=0
            echo "$(date +%T) busy: load $(cut -d' ' -f1-3 /proc/loadavg)" >> "$log"
            sleep 20
        else
            quiet=$((quiet + 1))
            sleep 10
        fi
    done
}

# Starts a monitor that appends a timestamp to $1 whenever a foreign process runs; prints its pid.
m2_start_intruder_monitor() {
    local file=$1
    # Detached from stdout, or a caller's command substitution would wait for the loop to end.
    ( while :; do pgrep -x "$M2_FOREIGN" >/dev/null && date +%T >> "$file"; sleep 2; done ) \
        > /dev/null 2>&1 &
    echo $!
}

# PIDs of the service this environment names. Other services of the same executable, such as a
# user's own, differ only in their socket, which only their environment shows.
m2_service_pids() {
    local pid
    for pid in $(pgrep -f "$LORE service run"); do
        tr '\0' '\n' < "/proc/$pid/environ" 2> /dev/null \
            | grep -qx "LORE_SERVICE_SOCKET=$LORE_SERVICE_SOCKET" && echo "$pid"
    done
}

m2_drop_caches() {
    sync && sudo -n /usr/sbin/sysctl -q vm.drop_caches=3
}

m2_now() {
    date +%s.%N
}

m2_elapsed() {
    echo "$2 - $1" | bc
}
