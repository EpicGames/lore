#!/usr/bin/env bash
# Runs a command once the machine has been quiet for QUIET_SECS (default 30) consecutive seconds,
# and restarts it whenever other processes interfere while it runs.
#
# Usage: quiet-run.sh <label> <command> [args...]
#
# The command runs under setsid with QUIET_RUN_ID=<unique id> in its environment. Every process it
# starts inherits that variable, even one that detaches into a session of its own (a service, a
# build server), so "the run" is every process whose environment carries the id, plus their
# descendants (a sudo child loses the variable). "Other processes" are all other user processes,
# excluding kernel threads, this script and QUIET_IGNORE.
#
# Quiet:        no process named like $QUIET_FOREIGN outside the run, and other processes under
#               QUIET_IDLE_TICKS (default 50, half a core) in each second. CPU is read per process
#               from /proc/<pid>/stat, in clock ticks (100 per core-second).
# Interference: such a process appears outside the run, or other processes use over
#               QUIET_BUSY_TICKS (default 200, two cores: desktop apps such as Slack or a terminal
#               spike near one core for a second) in one second, or over QUIET_IDLE_TICKS in 3
#               consecutive seconds. Every process of the run is killed, $QUIET_CLEANUP runs, and
#               the wait for quiet starts again. Leftover processes of the run are killed after a
#               kept run too.
#
# Logs go to $QUIET_LOG (default $QUIET_DIR/<label>.log, QUIET_DIR default /tmp/lore-fuse-m3/runs);
# a kept run's output goes to $QUIET_DIR/<label>-run-<n>.out. Gives up after QUIET_DEADLINE_SECS
# (default 36000). Exit status: the command's, or 75 on reaching the deadline.
set -u

label=$1
shift
QUIET_SECS=${QUIET_SECS:-30}
QUIET_IDLE_TICKS=${QUIET_IDLE_TICKS:-50}
QUIET_BUSY_TICKS=${QUIET_BUSY_TICKS:-200}
QUIET_DEADLINE_SECS=${QUIET_DEADLINE_SECS:-36000}
# Processes whose CPU the run itself causes, not counted as others: the VPN daemon carrying its traffic.
QUIET_IGNORE=${QUIET_IGNORE:-twingated}
QUIET_FOREIGN=${QUIET_FOREIGN:-'cargo|rustc|clippy-driver|pytest|cc1|cc1plus|ld|lld|mold|clang|clang++|gcc|g++|make|ninja|dotnet|UnrealBuildTool'}
dir=${QUIET_DIR:-/tmp/lore-fuse-m3/runs}
mkdir -p "$dir"
QUIET_LOG=${QUIET_LOG:-$dir/$label.log}
self_pid=$$
started=$(date +%s)
run_id=none

log() { echo "$(date +%T) $*" >> "$QUIET_LOG"; }

# Pids whose environment carries the current run id, one per line.
marked_pids() {
    [ "$run_id" = none ] && return 0
    grep -l -s -z -x "QUIET_RUN_ID=$run_id" /proc/[0-9]*/environ 2> /dev/null \
        | sed -n 's#^/proc/\([0-9]*\)/environ$#\1#p'
}

# The run: the marked pids and every descendant of one, one per line. A child that does not carry
# the mark (sudo resets the environment, so the cache drop's sysctl runs without it) is still the
# run's.
run_pids() {
    [ "$run_id" = none ] && return 0
    local marked
    marked=$(mktemp)
    marked_pids > "$marked"
    # Through cat: gawk stops at a file that vanished since the glob, and a build churns processes.
    cat /proc/[0-9]*/stat 2> /dev/null | awk -v markfile="$marked" '
        BEGIN { while ((getline p < markfile) > 0) run[p] = 1 }
        {
            pid = substr($0, 1, index($0, " ") - 1)
            rest = substr($0, index($0, ") ") + 2)
            split(rest, f, " ")
            parent[pid] = f[2]
        }
        END {
            do {
                grew = 0
                for (pid in parent) if (!(pid in run) && (parent[pid] in run)) { run[pid] = 1; grew = 1 }
            } while (grew)
            for (pid in run) print pid
        }'
    rm -f "$marked"
}

# Prints "<pid> <ticks>" for every user process outside the run, the kernel and this script.
other_ticks() {
    local skip
    skip=$(mktemp)
    run_pids > "$skip"
    cat /proc/[0-9]*/stat 2> /dev/null | awk -v self="$self_pid" -v skipfile="$skip" -v ignore="$QUIET_IGNORE" '
        BEGIN {
            while ((getline p < skipfile) > 0) skip[p] = 1
            n = split(ignore, names, "|"); for (i = 1; i <= n; i++) ignored[names[i]] = 1
        }
        {
            # The command name may hold spaces: fields count from the closing parenthesis.
            line = $0
            pid = substr(line, 1, index(line, " ") - 1)
            comm = substr(line, index(line, "(") + 1, index(line, ") ") - index(line, "(") - 1)
            rest = substr(line, index(line, ") ") + 2)
            split(rest, f, " ")
            ppid = f[2]; utime = f[12]; stime = f[13]
            if (pid == 2 || ppid == 2 || pid == self || ppid == self || (pid in skip) || (comm in ignored)) next
            print pid, utime + stime
        }'
    rm -f "$skip"
}

# Prints the first build-like process outside the run as "<pid> <name>", or nothing. Each
# candidate's own environment is read after it is found, so a process the run starts between a
# listing of the run and the name search still counts as the run's.
foreign_process() {
    local pid mine
    mine=$(run_pids)
    for pid in $(pgrep -x "$QUIET_FOREIGN"); do
        [ "$run_id" != none ] && grep -q -s -z -x "QUIET_RUN_ID=$run_id" "/proc/$pid/environ" && continue
        grep -qx "$pid" <<< "$mine" && continue
        # Gone since pgrep listed it, or a zombie (its environment reads empty): not a process
        # outside the run.
        [ -s "/proc/$pid/environ" ] || [ -n "$(head -c1 "/proc/$pid/environ" 2> /dev/null)" ] || continue
        echo "$pid $(cat "/proc/$pid/comm" 2> /dev/null) $(tr '\0' ' ' < "/proc/$pid/cmdline" 2> /dev/null | cut -c1-120)"
        return 0
    done
    return 1
}

# Sums the ticks other processes used since the previous sample; state lives in $1 (a file).
ticks_since() {
    local state=$1 now
    now=$(mktemp)
    other_ticks > "$now"
    awk -v deltas="$state.deltas" 'NR == FNR { before[$1] = $2; next }
         { if ($1 in before) d = $2 - before[$1]; else d = 0; if (d > 0) { sum += d; print $1, d > deltas } }
         END { print sum + 0 }' "$state" "$now"
    mv "$now" "$state"
}

# The heaviest other processes of the last sample in $1's state: "<pid>:<comm>:<ticks>", up to 5.
top_others() {
    local pid ticks
    sort -k2,2nr "$1.deltas" 2> /dev/null | head -5 | while read -r pid ticks; do
        printf '%s:%s:%s ' "$pid" "$(cat "/proc/$pid/comm" 2> /dev/null || echo gone)" "$ticks"
    done
}

kill_run() {
    local pid
    for pid in $(run_pids); do kill -TERM "$pid" 2> /dev/null; done
    sleep 2
    for pid in $(run_pids); do kill -KILL "$pid" 2> /dev/null; done
}

wait_quiet() {
    local state quiet=0 used offender
    state=$(mktemp)
    other_ticks > "$state"
    while [ "$quiet" -lt "$QUIET_SECS" ]; do
        if [ $(( $(date +%s) - started )) -ge "$QUIET_DEADLINE_SECS" ]; then
            rm -f "$state"
            return 1
        fi
        sleep 1
        used=$(ticks_since "$state")
        offender=$(foreign_process)
        if [ -n "$offender" ] || [ "$used" -ge "$QUIET_IDLE_TICKS" ]; then
            [ "$quiet" -gt 0 ] && log "wait: not quiet after ${quiet}s (others ${used} ticks/s${offender:+, build process $offender})"
            quiet=0
        else
            quiet=$((quiet + 1))
        fi
    done
    rm -f "$state"
    return 0
}

attempt=0
while :; do
    run_id=none
    if ! wait_quiet; then
        log "deadline reached after $attempt attempts"
        exit 75
    fi
    attempt=$((attempt + 1))
    run_id=$label-$$-$attempt-$(date +%s%N)
    out=$dir/$label-attempt-$attempt.out
    log "attempt $attempt: quiet for ${QUIET_SECS}s, starting: $*"
    QUIET_RUN_ID=$run_id setsid "$@" > "$out" 2>&1 &
    run=$!
    sleep 0.2
    state=$(mktemp)
    other_ticks > "$state"
    over=0 interfered=""
    while kill -0 "$run" 2> /dev/null; do
        sleep 1
        kill -0 "$run" 2> /dev/null || break
        used=$(ticks_since "$state")
        offender=$(foreign_process)
        if [ -n "$offender" ]; then
            interfered="a build process outside the run: $offender"
        elif [ "$used" -ge "$QUIET_BUSY_TICKS" ]; then
            interfered="others used ${used} ticks in one second"
        elif [ "$used" -ge "$QUIET_IDLE_TICKS" ]; then
            over=$((over + 1))
            [ "$over" -ge 3 ] && interfered="others over ${QUIET_IDLE_TICKS} ticks/s for 3 seconds"
        else
            over=0
        fi
        if [ -n "$interfered" ]; then
            log "attempt $attempt: interference: $interfered (top others: $(top_others "$state")); killing the run"
            kill_run
            wait "$run" 2> /dev/null
            break
        fi
    done
    rm -f "$state" "$state.deltas"
    if [ -n "$interfered" ]; then
        [ -n "${QUIET_CLEANUP:-}" ] && bash -c "$QUIET_CLEANUP" >> "$QUIET_LOG" 2>&1
        continue
    fi
    wait "$run"
    status=$?
    leftover=$(run_pids | wc -l)
    [ "$leftover" -gt 0 ] && log "attempt $attempt: killing $leftover leftover processes of the run"
    kill_run
    kept=$(find "$dir" -maxdepth 1 -name "$label-run-*.out" | wc -l)
    mv "$out" "$dir/$label-run-$((kept + 1)).out"
    log "attempt $attempt: finished undisturbed, exit $status, kept as $label-run-$((kept + 1)).out"
    exit $status
done
