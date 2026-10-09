#!/bin/bash
# Runs inside the virtme-ng guest as root: sets up an ext4 disk and runs the suite's tests with
# the daemon and every program as uid 1000. Usage: guest.sh <bin-dir> [<group>…]
#
# The groups: `service`, the daemon's service command by command, whose lines are read by a
# person; `file_writes` (file_writes.sh), scenarios with checks; `cli`, `konedrivectl`'s test
# of a mounted folder. ONLY, when set, names the scenarios to run of the groups asked for.
set -u
bin=$1
shift
tests=${*:-service file_writes cli}
here=$(cd "$(dirname "$0")" && pwd)
. "$here/scenario_checks.sh"
. "$here/scenario_folder.sh"
. "$here/file_writes.sh"

modprobe loop
modprobe fuse 2>/dev/null
mount -t fusectl none /sys/fs/fuse/connections 2>/dev/null
mount -t tmpfs -o size=64M tmpfs /mnt
mkdir -p /mnt/ext4 /mnt/bin
# The disk is a file on the host's NVMe, opened by QEMU with O_DIRECT (run.sh): what is written
# here goes to the real disk and the host caches none of it.
disk=$(lsblk -dn -o NAME,TYPE | awk '$2=="disk" && $1 ~ /^(vd|sd)/ {print "/dev/" $1; exit}')
echo "disk: $disk $(lsblk -dn -o SIZE,MODEL $disk)"
mkfs.ext4 -q -F $disk
mount $disk /mnt/ext4

echo "root mount options: $(findmnt -no OPTIONS /)"
# The guest's root is the host over virtiofs; if it is nosuid, fusermount3's setuid bit is dead.
cp "$(command -v fusermount3)" /mnt/bin/fusermount3
chmod 4755 /mnt/bin/fusermount3
export PATH=/mnt/bin:$PATH
chmod 666 /dev/fuse

W=/mnt/ext4
mkdir -p $W/plain $W/logs
chown -R 1000:1000 $W/plain $W/logs
U="setpriv --reuid 1000 --regid 1000 --clear-groups env PATH=$PATH HOME=$W/plain"
# Root cannot stat another user's FUSE mount, so the mount table is asked.
mounted() { grep -q " $1 fuse" /proc/self/mounts; }

alive() { [ -d /proc/$1 ] && [ "$(awk '{print $3}' /proc/$1/stat 2>/dev/null)" != Z ]; }

# The daemon's own service with a folder served through FUSE, over the fake OneDrive.
service() {
    echo "==== the daemon's service with a folder served through FUSE ===="
    local f=$W/OneDrive3 st=$W/state3 src=$W/src3 svc
    ip link set lo up
    mkdir -p $f $st $src
    echo "hello" > $src/a; echo "inside docs" > $src/b; head -c 20971520 /dev/urandom > $src/big
    echo "hello, changed in the cloud" > $src/a2; echo "arrived later" > $src/new
    chown -R 1000:1000 $f $st $src
    mkfifo $st/in; chown 1000:1000 $st/in
    $U env RUST_LOG=info "$bin/fuse-service" $st < $st/in > $W/logs/svc.out 2>> $W/logs/svc.err &
    svc=$!
    exec 8> $st/in
    ask() {
        echo "$*" >&8
        for _ in $(seq 150); do
            if grep -qF "done $*:" $W/logs/svc.out; then grep -F "done $*:" $W/logs/svc.out | tail -1 | sed "s/^done [^:]*: *//"; return; fi
            sleep 0.1
        done
        echo "NO ANSWER"
    }
    for _ in $(seq 50); do grep -q ready $W/logs/svc.out && break; sleep 0.1; done
    ask file A R a.txt $src/a > /dev/null; ask folder D R docs > /dev/null
    ask file B D b.txt $src/b > /dev/null; ask file X D big.bin $src/big > /dev/null
    echo "register: $(ask register $f)"
    for _ in $(seq 100); do mounted $f && break; sleep 0.1; done
    mounted $f || { echo "NOT MOUNTED"; cat $W/logs/svc.err | tail -20; return; }
    sleep 1
    echo "type: $(awk -v m=$f '$2==m{print $3}' /proc/self/mounts)"
    echo "listing: $($U find $f | sort | tr '\n' ' ')"
    echo "status: $(ask status)"
    echo "root attributes: $($U python3 -c "import os,sys; print(sorted(os.listxattr(sys.argv[1])))" $f)"
    echo "state a.txt: $(ask state $f/a.txt); a.txt: $($U cat $f/a.txt)"; sleep 1; echo "state a moment after it was read whole: $(ask state $f/a.txt)"
    echo "-- a look at the start of a big file"
    echo "the first 32 KiB equal to the cloud's: $(cmp <($U head -c 32768 $f/docs/big.bin) <(head -c 32768 $src/big) && echo yes); state after: $(ask state $f/docs/big.bin); on disk: $($U stat -c %b $f/docs/big.bin) blocks"
    echo "bytes from the middle equal: $(cmp <($U dd if=$f/docs/big.bin bs=4096 skip=2000 count=4 2> /dev/null) <(dd if=$src/big bs=4096 skip=2000 count=4 2> /dev/null) && echo yes); state: $(ask state $f/docs/big.bin)"
    echo "read whole, while it downloads behind: equal to the cloud's: $($U cmp $f/docs/big.bin $src/big && echo yes)"; sleep 1; echo "state a moment after: $(ask state $f/docs/big.bin)"
    echo "freed again: $(ask dehydrate $f/docs/big.bin)"
    echo "hydrate big.bin: $(ask hydrate $f/docs/big.bin); equal to the cloud's: $($U cmp $f/docs/big.bin $src/big && echo yes)"
    echo "menu big.bin: $(ask menu $f/docs/big.bin)"
    echo "dehydrate big.bin: $(ask dehydrate $f/docs/big.bin); state: $(ask state $f/docs/big.bin)"
    echo "pin docs: $(ask pin $f/docs)"; sleep 1
    echo "states under the pin: b.txt $(ask state $f/docs/b.txt), big.bin $(ask state $f/docs/big.bin)"
    echo "dehydrate a pinned file: $(ask dehydrate $f/docs/b.txt)"
    echo "menu b.txt: $(ask menu $f/docs/b.txt)"
    echo "free-up docs (takes its pin off): $(ask free-up $f/docs)"
    echo "status: $(ask status)"
    $U "$bin/watch-client" $f 7 > $W/logs/watch3.out &
    w=$!
    sleep 1
    ask file N R new.txt $src/new > /dev/null; ask edit A $src/a2 > /dev/null; ask rename B R b-moved.txt > /dev/null
    echo "refresh: $(ask refresh)"
    wait $w
    echo "an inotify watch on the folder saw: $(tr '\n' ';' < $W/logs/watch3.out)"
    echo "listing: $($U find $f | sort | tr '\n' ' ')"
    echo "a.txt: $($U cat $f/a.txt); new.txt: $($U cat $f/new.txt)"; sleep 1
    echo "-- a waiting open, interrupted"
    echo "slow to arrive" > $src/slow; chown 1000:1000 $src/slow
    ask file S R slow.txt $src/slow > /dev/null; ask refresh > /dev/null; sleep 1
    ask delay GET dl/S 6000 1 > /dev/null
    $U cat $f/slow.txt > /dev/null 2>&1 &
    c=$!
    sleep 1
    alive $c && echo "the reader waits for its bytes; state meanwhile: $(ask state $f/slow.txt)" || echo "the reader did not wait"
    # Not SIGINT: a background command of a script ignores it.
    kill -TERM $c; sleep 1
    alive $c && echo "1 s after SIGTERM: still waiting" || echo "1 s after SIGTERM: gone"
    sleep 6
    echo "state afterwards (a read given up downloads nothing): $(ask state $f/slow.txt); read now: $($U cat $f/slow.txt)"
    echo "-- a file that is read goes before a pinned folder"
    ask folder Q R queue > /dev/null
    for n in $(seq -w 1 40); do ask file Q$n Q q$n.txt $src/new > /dev/null; done
    head -c 4194304 /dev/urandom > $src/first; chown 1000:1000 $src/first
    ask file F R first.bin $src/first > /dev/null; ask refresh > /dev/null; sleep 1
    # As the user: root cannot look into the mount.
    hydrated() { $U python3 -c "import os,sys; d=sys.argv[1]; print(sum(os.getxattr(os.path.join(d,n),'user.konedrive.state')==b'hydrated' for n in os.listdir(d)))" $f/queue; }
    ask delay GET dl/F 3000 1 > /dev/null
    $U cat $f/first.bin > /dev/null &
    c=$!
    sleep 0.5
    echo "pin the folder of 40 files while the file is read: $(ask pin $f/queue)"
    sleep 1.5
    echo "pinned files downloaded while the read still waits: $(hydrated) of 40"
    wait $c; sleep 4
    echo "pinned files downloaded once the read is over: $(hydrated) of 40; the file read: $($U cmp $f/first.bin $src/first > /dev/null 2>&1 && echo whole)"
    echo "-- a download cut off, and continued"
    head -c 41943040 /dev/urandom > $src/large; chown 1000:1000 $src/large
    ask file L R large.bin $src/large > /dev/null; ask refresh > /dev/null; sleep 1
    ask cut L 20971520 6 > /dev/null
    echo "first read: $($U cat $f/large.bin 2>&1 > /dev/null | sed 's/.*: //'); what it left (size, checkpoint): $(ask staged)"
    echo "downloads asked of the cloud so far: $(ask count GET dl/L)"; ask cut L 0 0 > /dev/null
    for _ in 1 2 3 4 5 6; do $U cat $f/large.bin > /dev/null 2>&1 && break; done
    echo "a later read equal to the cloud's: $($U cmp $f/large.bin $src/large && echo yes); left: $(ask staged); downloads asked of the cloud: $(ask count GET dl/L)"
    echo "-- a large pinned file, in parts"
    head -c 314572800 /dev/urandom > $src/huge; chown 1000:1000 $src/huge
    ask folder K R keep > /dev/null; ask file H K huge.bin $src/huge > /dev/null; ask refresh > /dev/null; sleep 1
    echo "pin keep: $(ask pin $f/keep)"
    for _ in $(seq 60); do [ "$(ask state $f/keep/huge.bin)" = hydrated ] && break; sleep 0.5; done
    echo "state: $(ask state $f/keep/huge.bin); equal to the cloud's: $($U cmp $f/keep/huge.bin $src/huge && echo yes); downloads asked of the cloud: $(ask count GET dl/H)"
    echo "unpin and free: $(ask free-up $f/keep)"
    echo "-- the daemon killed and started again"
    kill -KILL $svc; wait $svc 2> /dev/null; exec 8>&-
    echo "killed: $($U ls $f 2>&1)"
    # What the unit runs after the daemon's end: the dead mount goes and the folder is locked.
    $U "$bin/fuse-service" --tidy $f
    echo "the folder itself, the daemon dead: $($U find $f | sort | tr '\n' ' ')"
    echo "its modes: $($U stat -c '%a' $f $f/docs $f/a.txt | tr '\n' ' '); a file put into it: $($U sh -c "echo x > $f/mine.txt" 2>&1 | sed 's/.*: //'); a downloaded file read: $($U cat $f/a.txt)"
    # Only on purpose: the user unlocks a directory and puts a file of their own where an item
    # that is not downloaded has its name.
    $U sh -c "chmod u+w $f/docs && echo 'the user own file' > $f/docs/big.bin"
    : > $W/logs/svc.out
    $U env RUST_LOG=info "$bin/fuse-service" $st < $st/in > $W/logs/svc.out 2>> $W/logs/svc.err &
    svc=$!
    exec 8> $st/in
    for _ in $(seq 100); do grep -q ready $W/logs/svc.out && break; sleep 0.1; done
    sleep 1
    mounted $f && echo "mounted again: $($U ls $f | tr '\n' ' ')" || echo "NOT MOUNTED AGAIN: $(tail -5 $W/logs/svc.err)"
    echo "a downloaded file after the restart: state $(ask state $f/a.txt), content: $($U cat $f/a.txt)"
    echo "status: $(ask status)"
    echo "an item whose name the user's own file took: state $(ask state $f/docs/big.bin), size $($U stat -c %s $f/docs/big.bin), read: $($U cat $f/docs/big.bin 2>&1 | head -c 80)"
    echo "free-up-space: $(ask free-up-space)"
    echo "activity: $(ask activity | head -c 900)"
    echo "forget: $(ask forget)"; sleep 1
    mounted $f && echo "still mounted after the forget" || echo "the folder itself after the forget: $($U find $f | sort | tr '\n' ' ')"
    echo "its modes after the forget, the user's again: $($U stat -c '%a' $f $f/docs | tr '\n' ' ')"
    echo "the user's own file after it all: $($U cat $f/docs/big.bin 2>&1)"
    exec 8>&-; wait $svc 2> /dev/null
    grep -E " (WARN|ERROR) " $W/logs/svc.err | tail -8
}

# The daemon on a private bus, `konedrivectl` and plain file calls on a folder served through
# FUSE: crates/konedrivectl/tests/sync_cli/fuse.rs.
cli() {
    echo "==== a folder served through FUSE, driven from the command line ===="
    ip link set lo up
    mkdir -p $W/tmp; chown 1000:1000 $W/tmp
    $U env TMPDIR=$W/tmp "$CLI_TEST" --ignored --exact fuse::a_folder_served_through_fuse_is_driven_from_the_command_line --nocapture 2>&1 | tail -25
}

# run_group <group>: the group, and beside it a watch that ends it when it does not end by
# itself within GROUP_SECONDS: a daemon that answers nothing any more holds every call on its
# folder for ever. The watch prints the end of the daemon's logs and kills the suite's
# programs, so that the calls return and the run goes on; the group is then counted as failed.
run_group() {
    local group=$1 overran=$W/logs/$1.overran watch
    (
        sleep "${GROUP_SECONDS:-1200}"
        touch "$overran"
        echo "==== $group did not end within ${GROUP_SECONDS:-1200} s: its programs are killed ===="
        for log in $(find $W/logs -name '*.err' -mmin -30); do
            echo "  the end of $log:"
            tail -30 "$log" | sed 's/^/    /'
        done
        pkill -KILL -x fuse-service
        pkill -KILL -x fake-onedrive
        pkill -KILL -f sync_cli-
    ) &
    watch=$!
    $group
    kill $watch 2> /dev/null
    [ -e "$overran" ] && FAILED_SCENARIOS+=("$group: it did not end by itself")
}

for t in $tests; do
    run_group $t
done
summarize_scenarios
echo "guest-done"
