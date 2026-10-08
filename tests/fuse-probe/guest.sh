#!/bin/bash
# Runs inside the virtme-ng guest as root: sets up an ext4 image and runs the probe's tests with
# the daemon and every program as uid 1000. Usage: guest.sh <bin-dir> [t1 t2 t3 t4 t5]
set -u
bin=$1
shift
tests=${*:-t1 t2 t3 t4 t5}

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
mkdir -p $W/backing $W/mnt $W/plain $W/logs
chown -R 1000:1000 $W/backing $W/mnt $W/plain $W/logs
U="setpriv --reuid 1000 --regid 1000 --clear-groups env PATH=$PATH HOME=$W/plain"
DPID=

# Root cannot stat another user's FUSE mount, so the mount table is asked.
mounted() { grep -q " $1 fuse" /proc/self/mounts; }

start() { # start [probe options]
    $U "$bin/fuse-probe" $W/backing $W/mnt "$@" 2> $W/logs/daemon.err &
    DPID=$!
    for _ in $(seq 50); do
        mounted $W/mnt && return 0
        sleep 0.1
    done
    echo "MOUNT FAILED: $(cat $W/logs/daemon.err)"
    return 1
}

stop() {
    $U fusermount3 -u $W/mnt 2>&1
    wait $DPID 2>/dev/null
    DPID=
}

alive() { [ -d /proc/$1 ] && [ "$(awk '{print $3}' /proc/$1/stat 2>/dev/null)" != Z ]; }

t1() {
    echo "==== T1 mapped writes ===="
    for v in msync munmap closefirst exit kill; do
        start --log $W/logs/t1-$v.log || return
        $U "$bin/probe-client" mmap $W/mnt/map-$v $v
        echo "client exit: $?"
        sleep 3
        size=$(stat -c %s $W/backing/map-$v)
        other=$(tr -d Z < $W/backing/map-$v | wc -c)
        echo "$v: backing size=$size bytes-not-stored=$other"
        # The order of requests for the file, runs of one kind folded.
        awk '{ k=$2; if (k=="write") k=k"("$NF")"; if (k!=p) { if (p!="") printf "%s x%d, ", p, n; p=k; n=0 } n++ }
             END { printf "%s x%d\n", p, n }' $W/logs/t1-$v.log
        first_write=$(awk '$2=="write"{print $1; exit}' $W/logs/t1-$v.log)
        last_write=$(awk '$2=="write"{t=$1} END{print t}' $W/logs/t1-$v.log)
        release=$(awk '$2=="release"{t=$1} END{print t}' $W/logs/t1-$v.log)
        echo "$v: first write $first_write, last write $last_write, last release $release (ms)"
        stop
    done
}

t2() {
    echo "==== T2 a waiting open ===="
    start --cloud 10 --fill-ms 15000 --log $W/logs/t2.log || return
    $U cat $W/mnt/cloud/f00000.bin > /dev/null &
    c1=$!
    sleep 1
    s=$(date +%s.%N)
    $U stat -c 'stat of another file: size %s' $W/mnt/cloud/f00001.bin
    $U ls $W/mnt > /dev/null
    echo "other requests took $(echo "$(date +%s.%N) - $s" | bc) s while an open waits"
    kill -INT $c1
    sleep 1
    alive $c1 && echo "first opener: alive 1 s after SIGINT" || echo "first opener: gone after SIGINT"
    kill -KILL $c1
    sleep 1
    alive $c1 && echo "first opener: alive 1 s after SIGKILL" || echo "first opener: gone after SIGKILL"
    $U cat $W/mnt/cloud/f00002.bin > /dev/null &
    c2=$!
    sleep 1
    kill -KILL $c2
    sleep 1
    alive $c2 && echo "second opener: alive 1 s after SIGKILL" || echo "second opener: gone after SIGKILL"
    wait $c1 $c2 2>/dev/null
    echo "both openers ended after the fill; fills in the log: $(grep -c filled $W/logs/t2.log)"
    grep -i interrupt $W/logs/daemon.err | head -3
    stop
}

t3() {
    echo "==== T3 daemon killed, daemon hung ===="
    echo "-- a 0555 mount point"
    mkdir $W/ro
    chown 1000:1000 $W/ro
    chmod 555 $W/ro
    $U "$bin/fuse-probe" $W/backing $W/ro 2>&1 &
    p=$!
    sleep 1
    if mounted $W/ro; then
        echo "0555 mount point: mounted"
        $U fusermount3 -u $W/ro
    else
        echo "0555 mount point: refused"
    fi
    wait $p 2>/dev/null

    echo "-- SIGKILL of the daemon"
    start --cloud 10 --fill-ms 60000 || return
    $U sh -c "cd $W/mnt && exec sleep 600" &
    holder=$!
    $U dd if=/dev/zero of=$W/mnt/big bs=1M count=3000 2> $W/logs/dd.err &
    writer=$!
    $U cat $W/mnt/cloud/f00000.bin > /dev/null 2> $W/logs/cat.err &
    opener=$!
    sleep 1
    kill -KILL $DPID
    wait $DPID 2>/dev/null
    sleep 1
    alive $writer && echo "writer: still alive" || echo "writer: ended: $(tail -1 $W/logs/dd.err | head -c 200)"
    grep -v records $W/logs/dd.err | head -2
    alive $opener && echo "waiting opener: still alive" || echo "waiting opener: ended: $(cat $W/logs/cat.err)"
    echo "ls: $($U ls $W/mnt 2>&1)"
    echo "backing/big after the kill: $(stat -c %s $W/backing/big) bytes"
    echo "fusermount3 -u: $($U fusermount3 -u $W/mnt 2>&1; echo rc=$?)"
    echo "fusermount3 -u -z: $($U fusermount3 -u -z $W/mnt 2>&1; echo rc=$?)"
    start && echo "mounted again by the same user; ls: $($U ls $W/mnt | tr '\n' ' ')"
    echo "shell that sat in the old mount: $(ls /proc/$holder/cwd 2>&1 | head -1)"
    kill $holder
    wait $writer $opener $holder 2>/dev/null

    echo "-- hung daemon"
    conn=$(( $($U stat -c %d $W/mnt) & 0xff ))
    echo "connection $conn: $(ls -l /sys/fs/fuse/connections/$conn/abort 2>&1)"
    $U ls $W/mnt > /dev/null
    kill -USR1 $DPID
    sleep 2
    $U stat $W/mnt/fresh-name > /dev/null 2> $W/logs/stat.err &
    hung=$!
    sleep 2
    alive $hung && echo "stat on a hung daemon: waiting" || echo "stat on a hung daemon: returned"
    kill -KILL $hung
    sleep 1
    alive $hung && echo "stat after SIGKILL: still waiting" || echo "stat after SIGKILL: gone"
    $U sh -c "echo 1 > /sys/fs/fuse/connections/$conn/abort" && echo "owner wrote abort"
    sleep 1
    alive $hung && echo "stat after abort: still waiting" || echo "stat after abort: gone ($(cat $W/logs/stat.err))"
    echo "ls after abort: $($U ls $W/mnt 2>&1)"
    kill -KILL $DPID
    wait $DPID 2>/dev/null
    $U fusermount3 -u $W/mnt
    rm -f $W/backing/big
}

rate() { tail -1 | sed 's/.*, //'; }

bench_io() { # bench_io label dir
    local f=$2/bench.bin
    echo "$1 write 1M, to disk (fdatasync): $($U dd if=/dev/zero of=$f bs=1M count=4096 conv=fdatasync 2>&1 | rate)"
    echo 3 > /proc/sys/vm/drop_caches
    echo "$1 read 1M, from disk: $($U dd if=$f of=/dev/null bs=1M 2>&1 | rate)"
    echo "$1 read 1M, again: $($U dd if=$f of=/dev/null bs=1M 2>&1 | rate)"
    $U rm -f $f
    sync
    echo "$1 write 1M, as cp does (no sync): $($U dd if=/dev/zero of=$f bs=1M count=4096 2>&1 | rate)"
    sync
    $U rm -f $f
    echo "$1 write 4k, to disk (fdatasync): $($U dd if=/dev/zero of=$f bs=4k count=262144 conv=fdatasync 2>&1 | rate)"
    echo 3 > /proc/sys/vm/drop_caches
    echo "$1 read 4k, from disk: $($U dd if=$f of=/dev/null bs=4k 2>&1 | rate)"
    $U rm -f $f
    sync
}

t4() {
    echo "==== T4 throughput (4 GiB with 1 MiB blocks, 1 GiB with 4 KiB blocks; ext4 on the host's NVMe) ===="
    # The image is written once in full first: a first write into a fresh image costs the host
    # extra, and whoever ran first would pay it.
    dd if=/dev/zero of=$W/plain/prefill bs=1M count=20000 conv=fdatasync 2> /dev/null
    rm -f $W/plain/prefill
    sync
    fstrim $W 2> /dev/null
    bench_io ext4 $W/plain
    start || return
    bench_io fuse $W/mnt
    stop
    start --direct-io 1 || return
    bench_io "fuse direct_io" $W/mnt
    stop
    bench_io "ext4 again" $W/plain
}

timeit() { # timeit label command...
    local label=$1 s e
    shift
    s=$(date +%s.%N)
    "$@" > /dev/null 2> $W/logs/timeit.err
    local rc=$?
    e=$(date +%s.%N)
    printf '%s: %.2f s' "$label" "$(echo "$e - $s" | bc)"
    [ $rc -eq 0 ] && echo || echo " FAILED rc=$rc: $(head -c 200 $W/logs/timeit.err)"
}

bench_meta() { # bench_meta label dir
    local d=$2/tree
    timeit "$1 cp -a of the tree" $U cp -a $W/src $d
    echo 3 > /proc/sys/vm/drop_caches
    timeit "$1 git status cold" $U git -C $d status
    timeit "$1 git status warm" $U git -C $d status
    echo 3 > /proc/sys/vm/drop_caches
    timeit "$1 tar cold" $U sh -c "tar cf - -C $d . | cat"
    timeit "$1 find warm" $U find $d -type f
    timeit "$1 git checkout of 5,000 changed files" $U git -C $d checkout -q second
    timeit "$1 rm -rf" $U rm -rf $d
}

t5() {
    echo "==== T5 metadata (50,000 files in 500 directories) ===="
    mkdir $W/src
    chown 1000:1000 $W/src
    $U python3 - $W/src << 'EOF'
import os, sys
root = sys.argv[1]
for d in range(500):
    os.mkdir(f"{root}/d{d:03}")
    for f in range(100):
        with open(f"{root}/d{d:03}/f{f:03}.txt", "w") as h:
            h.write(f"file {d} {f}\n" * 40)
EOF
    G="$U git -C $W/src -c user.name=probe -c user.email=probe@example.invalid -c gc.auto=0 -c maintenance.auto=false"
    $G init -q -b first
    $G add -A
    $G commit -q -m first
    $G checkout -q -b second
    for d in $(seq -w 0 49); do
        for f in $W/src/d0$d/*.txt; do echo changed >> $f; done
    done
    $G commit -q -a -m second
    $G checkout -q first
    bench_meta ext4 $W/plain
    start || return
    bench_meta fuse $W/mnt
    stop
    start --ttl-ms 60000 || return
    bench_meta "fuse ttl=60s" $W/mnt
    stop
}

subtype() {
    echo "==== mount type ===="
    start || return
    echo "without subtype: $(awk -v m=$W/mnt '$2==m{print $1, $3}' /proc/self/mounts)"
    stop
    start --subtype konedrive || return
    echo "with subtype=konedrive: $(awk -v m=$W/mnt '$2==m{print $1, $3}' /proc/self/mounts)"
    stop
}

for t in $tests; do
    $t
done
echo "guest-done"
