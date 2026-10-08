#!/bin/sh
# A trial of the daemon of this branch on this machine, with folders served through FUSE, and
# nothing of the installed konedrive changed: the installed daemon is stopped for the trial and
# started again after it, and the trial keeps its own config.toml and state in
# ~/.konedrive-fuse-trial. The window, the tray, the Dolphin plugins and konedrivectl are the
# installed ones: they talk to whichever daemon holds the name on the session bus.
#
# Usage: tests/fuse-probe/try-on-host.sh start | stop | status
#
# After `start`: add the account (`konedrivectl account add`, or in the window), then register
# a NEW EMPTY folder (`konedrivectl sync register ~/OneDrive-fuse`). Never the folder of the
# installed konedrive: it holds placeholders.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
trial=$HOME/.konedrive-fuse-trial
pidfile=$trial/daemon.pid
log=$trial/daemon.log
installed_config=${XDG_CONFIG_HOME:-$HOME/.config}/konedrive/config.toml

running() { [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2> /dev/null; }

# The mounts the trial's daemon made.
mounts() { awk '$3 == "fuse.konedrive" { print $2 }' /proc/self/mounts; }

case ${1:-} in
    start)
        if running; then
            echo "the trial's daemon runs already (pid $(cat "$pidfile"))"
            exit 0
        fi
        cargo build --release --manifest-path "$repo/Cargo.toml" -p konedrived
        mkdir -p "$trial/config" "$trial/state" "$trial/data"
        # The installed daemon makes way, and is not started by the bus while the trial runs.
        systemctl --user stop konedrived.service
        systemctl --user mask --runtime konedrived.service
        KONEDRIVE_FUSE=1 XDG_CONFIG_HOME="$trial/config" XDG_STATE_HOME="$trial/state" XDG_DATA_HOME="$trial/data" \
            nohup "$repo/target/release/konedrived" > "$log" 2>&1 &
        echo $! > "$pidfile"
        sleep 2
        if ! running; then
            echo "the trial's daemon did not stay up; its log:"
            tail -20 "$log"
            "$0" stop
            exit 1
        fi
        # The same app registration as the installed konedrive signs in with.
        if ! grep -q '^client_id' "$trial/config/konedrive/config.toml" 2> /dev/null; then
            id=$(sed -n 's/^client_id *= *"\(.*\)"$/\1/p' "$installed_config" | head -1)
            if [ -n "$id" ]; then
                konedrivectl set-client-id "$id"
            else
                echo "no client_id found in $installed_config: set it with konedrivectl set-client-id"
            fi
        fi
        echo "the trial's daemon runs (pid $(cat "$pidfile")); its log: $log"
        echo "next: konedrivectl account add, then konedrivectl sync register <a new empty folder>"
        ;;
    stop)
        if running; then
            kill "$(cat "$pidfile")"
            for _ in 1 2 3 4 5 6 7 8 9 10; do
                running || break
                sleep 0.5
            done
            running && kill -KILL "$(cat "$pidfile")"
        fi
        rm -f "$pidfile"
        # A mount the daemon left behind is taken off: the folder then shows what was downloaded.
        for m in $(mounts); do
            fusermount3 -u -z "$m" || true
        done
        systemctl --user unmask --runtime konedrived.service
        systemctl --user start konedrived.service
        echo "the trial is over; the installed daemon runs again"
        ;;
    status)
        if running; then echo "the trial's daemon runs (pid $(cat "$pidfile"))"; else echo "the trial's daemon does not run"; fi
        echo "installed daemon: $(systemctl --user is-active konedrived.service || true)"
        echo "mounts: $(mounts | tr '\n' ' ')"
        [ -f "$log" ] && tail -15 "$log"
        ;;
    *)
        echo "usage: $0 start | stop | status" >&2
        exit 2
        ;;
esac
