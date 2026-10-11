# A scenario's folder: a folder served through FUSE by the daemon's service (`fuse-service`),
# over a fake OneDrive that runs as a process of its own (`fake-onedrive`) and so outlives a
# service that is killed. Sourced by guest.sh, which gives $W, $U, $bin and `mounted`.
#
# One folder at a time: `start_folder` makes it, `stop_folder` takes it down. $F is the
# folder, $SRC a directory beside it for what a scenario seeds and compares with.

SERVICE_PID=
ONEDRIVE_PID=
ONEDRIVE_ADDRESS=

# converse <fd> <answers file> <command…>: the command sent, and its answer waited for. The
# answers come in the order of the commands, so a command's answer is the next `done` line.
converse() {
    local fd=$1 answers=$2 before
    shift 2
    { true >&"$fd"; } 2> /dev/null || { echo "NO ANSWER: it does not run"; return; }
    before=$(grep -c '^done ' "$answers")
    echo "$*" >&"$fd"
    for _ in $(seq 1200); do
        if [ "$(grep -c '^done ' "$answers")" -gt "$before" ]; then
            grep '^done ' "$answers" | sed -n "$((before + 1))p" | sed 's/^done [^:]*: \{0,1\}//'
            return
        fi
        sleep 0.05
    done
    echo "NO ANSWER"
}

# A command of the daemon's service (`daemon`, not `ask`, which the `service` group defines
# for itself), and of the fake OneDrive.
daemon() { converse 8 "$LOGS/service.out" "$@"; }
onedrive() { converse 9 "$LOGS/onedrive.out" "$@"; }

# What a person or a program does, as the user, in the folder.
user() { $U sh -c "cd $F && $1"; }

# errno_of <python statement>: the errno a call in the folder ends with, by name; `none`
# when it goes through.
errno_of() {
    $U python3 -c "
import errno, os, sys
os.chdir(sys.argv[1])
try:
    $1
    print('none')
except OSError as e:
    print(errno.errorcode[e.errno])" "$F"
}

# state_of <path in the folder>: what the mount's `user.konedrive.state` says; `none` for a
# file that has no such attribute, which is one that is not in OneDrive.
state_of() {
    $U python3 -c "
import os, sys
try:
    print(os.getxattr(sys.argv[1], 'user.konedrive.state').decode())
except OSError:
    print('none')" "$F/$1"
}

# listing: every name of the folder, below it, sorted, on one line.
listing() { user "find . -mindepth 1 | sort | sed 's|^\./||' | tr '\n' ' ' | sed 's/ $//'"; }

# expected <text>: a file that holds the text, to compare with.
expected() {
    printf '%s' "$1" > "$SRC/expected"
    echo "$SRC/expected"
}

# seed_file <id> <parent id> <name> <text>, seed_folder <id> <parent id> <name>: what OneDrive
# holds before the folder is registered, or gets while it runs.
seed_file() {
    printf '%s' "$4" > "$SRC/seed-$1"
    onedrive file "$1" "$2" "$3" "$SRC/seed-$1" > /dev/null
}
seed_folder() { onedrive folder "$1" "$2" "$3" > /dev/null; }

# What a scenario waits for.
onedrive_holds() { [ "$(onedrive compare "$1" "$2")" = equal ]; }
onedrive_has() { [ "$(onedrive id "$1")" != absent ]; }
onedrive_lacks() { [ "$(onedrive id "$1")" = absent ]; }
outbox_is_empty() { [ -z "$(daemon outbox)" ]; }
outbox_has_rows() { [ -n "$(daemon outbox)" ]; }
state_is() { [ "$(state_of "$1")" = "$2" ]; }
folder_has() { user "test -e '$1'"; }
folder_lacks() { user "! test -e '$1'"; }

# The fake OneDrive, empty.
start_onedrive() {
    mkfifo "$WORLD/onedrive.in"
    chown 1000:1000 "$WORLD/onedrive.in"
    $U "$bin/fake-onedrive" < "$WORLD/onedrive.in" > "$LOGS/onedrive.out" 2> "$LOGS/onedrive.err" &
    ONEDRIVE_PID=$!
    exec 9> "$WORLD/onedrive.in"
    wait_for 10 grep -q '^ready ' "$LOGS/onedrive.out"
    ONEDRIVE_ADDRESS=$(sed -n 's/^ready //p' "$LOGS/onedrive.out")
}

# start_service [--read-write]: the service over the state directory, which brings up the
# folder its `config.toml` records, if it records one.
start_service() {
    : > "$LOGS/service.out"
    $U env RUST_LOG="${SERVICE_LOG:-info}" "$bin/fuse-service" "$WORLD/state" --onedrive "$ONEDRIVE_ADDRESS" "$@" \
        < "$WORLD/service.in" > "$LOGS/service.out" 2>> "$LOGS/service.err" &
    SERVICE_PID=$!
    exec 8> "$WORLD/service.in"
    wait_for 20 grep -q '^ready' "$LOGS/service.out"
}

first_cycle_is_over() { daemon status | grep -q 'last checked true'; }
takes_changes() { [ "$($U stat -c %a "$F" 2> /dev/null)" = 755 ]; }

# The folder mounted, its first cycle over and, read-write, taking changes.
wait_for_folder() {
    wait_for 20 mounted "$F" || { echo "    NOT MOUNTED: $(tail -5 "$LOGS/service.err")"; return 1; }
    wait_for 20 first_cycle_is_over
    [ "${1:-}" = read-only ] || wait_for 20 takes_changes
}

# begin_folder [small-disk]: the directories of the scenario that calls it, and its fake
# OneDrive, which the scenario seeds before `register_folder`. `small-disk`: the folder is on
# a filesystem of its own, of 64 MiB, which holds nothing else.
begin_folder() {
    WORLD=$W/scenarios/${FUNCNAME[1]}
    LOGS=$W/logs/${FUNCNAME[1]}
    F=$WORLD/OneDrive
    SRC=$WORLD/src
    rm -rf "$WORLD" "$LOGS"
    mkdir -p "$WORLD/state" "$SRC" "$LOGS"
    if [ "${1:-}" = small-disk ]; then
        mkdir "$WORLD/disk"
        truncate -s 64M "$WORLD/disk.img"
        mkfs.ext4 -q -F "$WORLD/disk.img"
        mount -o loop "$WORLD/disk.img" "$WORLD/disk"
        F=$WORLD/disk/OneDrive
    fi
    mkdir -p "$F"
    mkfifo "$WORLD/service.in"
    chown -R 1000:1000 "$WORLD" "$LOGS"
    start_onedrive
}

# register_folder [read-only]: the service started and the folder registered with it,
# read-write unless said otherwise.
register_folder() {
    if [ "${1:-}" = read-only ]; then start_service; else start_service --read-write; fi
    daemon register "$F" > /dev/null
    wait_for_folder "${1:-}"
}

# The service killed as a crash kills it; the dead mount stays on the folder.
kill_service() {
    kill -KILL "$SERVICE_PID"
    wait "$SERVICE_PID" 2> /dev/null
    exec 8>&-
}

# The service stopped as the daemon is stopped, by SIGTERM: how it says its stop ended
# (`Finished`, or `Bound` when the stop's bound passed first); nothing when it did not end.
stop_service() {
    kill -TERM "$SERVICE_PID"
    wait_for 20 service_has_ended
    wait "$SERVICE_PID" 2> /dev/null
    exec 8>&-
    sed -n 's/^stopped: //p' "$LOGS/service.out"
}

# What the unit runs after the daemon's end: the dead mount goes and the folder is locked.
tidy_folder_at_once() { $U "$bin/fuse-service" --tidy "$F"; }

# The same, once the kernel has forgotten what it remembered of the folder under the dead
# mount, which is a second: a scenario that is not about that second waits it out here, and
# `dead_mount_is_taken_off_at_once_after_a_kill` (daemon_restarts.sh) is about it.
tidy_folder() {
    wait_for 5 dead_mount_is_seen
    tidy_folder_at_once
}
dead_mount_is_seen() { ! mounted "$F" || $U stat "$F" 2>&1 | grep -q 'not connected'; }

# The service started again over the same state, and its folder waited for.
restart_service() {
    start_service
    wait_for_folder "${1:-}"
}

# The scenario's folder taken down: the service and the fake OneDrive ended.
stop_folder() {
    exec 8>&-
    if [ -n "$SERVICE_PID" ]; then
        wait_for 5 service_has_ended || kill -KILL "$SERVICE_PID"
        wait "$SERVICE_PID" 2> /dev/null
    fi
    mounted "$F" && tidy_folder
    mountpoint -q "$WORLD/disk" 2> /dev/null && umount "$WORLD/disk"
    exec 9>&-
    [ -n "$ONEDRIVE_PID" ] && wait "$ONEDRIVE_PID" 2> /dev/null
    SERVICE_PID=
    ONEDRIVE_PID=
}
service_has_ended() { ! [ -d "/proc/$SERVICE_PID" ] || [ "$(awk '{print $3}' "/proc/$SERVICE_PID/stat" 2> /dev/null)" = Z ]; }

# What a failed scenario leaves to read: the outbox, what OneDrive holds, the folder, and the
# end of the daemon's log.
print_folder_evidence() {
    echo "    the outbox: $(daemon outbox)"
    echo "    the outbox's counts: $(daemon outbox-status)"
    echo "    OneDrive holds: $(onedrive paths); its bin: $(onedrive bin)"
    echo "    the folder: $(listing 2>&1)"
    echo "    the daemon's log, its end:"
    tail -${EVIDENCE_LOG_LINES:-25} "$LOGS/service.err" | sed 's/^/      /'
}

# folder_scenario <function>: a scenario with a folder of its own, taken down whatever came
# of it.
folder_scenario() {
    is_chosen "$1" || return 0
    scenario "$1" print_folder_evidence
    stop_folder
}

# check_onedrive_holds <path> <text> [<seconds>]: OneDrive comes to hold the text at the path.
check_onedrive_holds() {
    local file
    file=$(expected "$2")
    wait_for "${3:-30}" onedrive_holds "$1" "$file"
    check "OneDrive holds $1 as it is here" "$(onedrive compare "$1" "$file")" equal
}

# check_folder_holds <path> <text>: the folder shows the text at the path.
check_folder_holds() { check "the folder shows $1 with its content" "$(user "cat '$1'" 2>&1)" "$2"; }

# check_sent: nothing waits to be uploaded any more.
check_sent() {
    wait_for "${1:-20}" outbox_is_empty
    check "nothing waits to be uploaded" "$(daemon outbox)" ""
}

# check_state <path> <state>: the mount comes to say the state of the path.
check_state() {
    wait_for 10 state_is "$1" "$2"
    check "the state of $1" "$(state_of "$1")" "$2"
}

# download <path>: the file read whole, and downloaded by that.
download() {
    user "cat '$1' > /dev/null"
    wait_for 10 state_is "$1" hydrated
}
