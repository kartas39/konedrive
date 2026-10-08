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

reread() { # reread label count-MiB [probe options]
    local label=$1 n=$2 f=$W/mnt/bench.bin pass
    shift 2
    start --log $W/logs/reread.log "$@" || return
    $U dd if=/dev/zero of=$f bs=1M count=$n conv=fdatasync 2> /dev/null
    echo 3 > /proc/sys/vm/drop_caches
    for pass in "from disk" again "third time"; do
        : > $W/logs/reread.log
        echo "$label read, $pass: $($U dd if=$f of=/dev/null bs=1M 2>&1 | rate); requests to the daemon: $(grep -c '^[0-9]* read' $W/logs/reread.log); backing file cached: $(fincore -n -o RES $W/backing/bench.bin)"
    done
    $U rm -f $f
    stop
}

t4b() {
    echo "==== T4b second read of a 4 GiB file in a guest with 8 GiB ===="
    reread "1 GiB, cached twice" 1024
    reread "4 GiB, cached twice" 4096
    reread "4 GiB, backing cache dropped" 4096 --drop-backing 1
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

over() {
    echo "==== mounted over the directory that holds the files ===="
    local d=$W/over
    mkdir $d
    chown 1000:1000 $d
    $U sh -c "echo local > $d/local.txt"
    $U "$bin/fuse-probe" $d $d --cloud 3 --cloud-size 5 2> $W/logs/over.err &
    p=$!
    sleep 1
    mounted $d || { echo "MOUNT FAILED: $(cat $W/logs/over.err)"; return; }
    echo "mounted, through the mount: $($U sh -c "ls $d $d/cloud" | tr '\n' ' ')"
    echo "a file not downloaded, read: $($U cat $d/cloud/f00000.bin)"
    $U sh -c "echo new > $d/new.txt; mkdir $d/sub; echo deep > $d/sub/deep.txt; mv $d/local.txt $d/sub/"
    $U dd if=/dev/zero of=$d/big bs=1M count=200 conv=fdatasync 2> /dev/null
    echo "after writes, through the mount: $($U find $d -type f | sort | tr '\n' ' ')"
    kill -KILL $p
    wait $p 2> /dev/null
    echo "daemon killed: $($U ls $d 2>&1)"
    $U fusermount3 -u -z $d
    echo "unmounted, the directory itself: $($U find $d -type f | sort | tr '\n' ' ')"
    $U sh -c "echo offline > $d/offline.txt"
    $U "$bin/fuse-probe" $d $d --cloud 3 --cloud-size 5 2> $W/logs/over.err &
    p=$!
    sleep 1
    echo "mounted again: $($U find $d -type f | sort | tr '\n' ' ')"
    $U fusermount3 -u $d
    wait $p 2> /dev/null
}

arrivals() {
    echo "==== changes from the cloud, seen by an inotify watch on the directory ===="
    start --cloud 4 --arrivals 1 || return
    echo "before: $($U ls $W/mnt/cloud | tr '\n' ' ')"
    $U "$bin/probe-client" watch $W/mnt/cloud 8
    echo "after: $($U ls $W/mnt/cloud | tr '\n' ' ')"
    cat $W/logs/daemon.err
    stop
}

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
    $U "$bin/fuse-service" $st < $st/in > $W/logs/svc.out 2> $W/logs/svc.err &
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
    echo "state a.txt: $(ask state $f/a.txt); a.txt: $($U cat $f/a.txt); state after: $(ask state $f/a.txt)"
    echo "hydrate big.bin: $(ask hydrate $f/docs/big.bin); equal to the cloud's: $($U cmp $f/docs/big.bin $src/big && echo yes)"
    echo "menu big.bin: $(ask menu $f/docs/big.bin)"
    echo "dehydrate big.bin: $(ask dehydrate $f/docs/big.bin); state: $(ask state $f/docs/big.bin)"
    echo "pin docs: $(ask pin $f/docs)"; sleep 1
    echo "states under the pin: b.txt $(ask state $f/docs/b.txt), big.bin $(ask state $f/docs/big.bin)"
    echo "dehydrate a pinned file: $(ask dehydrate $f/docs/b.txt)"
    echo "menu b.txt: $(ask menu $f/docs/b.txt)"
    echo "free-up docs (takes its pin off): $(ask free-up $f/docs)"
    echo "status: $(ask status)"
    $U "$bin/probe-client" watch $f 7 > $W/logs/watch3.out &
    w=$!
    sleep 1
    ask file N R new.txt $src/new > /dev/null; ask edit A $src/a2 > /dev/null; ask rename B R b-moved.txt > /dev/null
    echo "refresh: $(ask refresh)"
    wait $w
    echo "an inotify watch on the folder saw: $(tr '\n' ';' < $W/logs/watch3.out)"
    echo "listing: $($U find $f | sort | tr '\n' ' ')"
    echo "a.txt: $($U cat $f/a.txt); new.txt: $($U cat $f/new.txt)"
    echo "-- a waiting open, interrupted"
    echo "slow to arrive" > $src/slow; chown 1000:1000 $src/slow
    ask file S R slow.txt $src/slow > /dev/null; ask refresh > /dev/null; sleep 1
    ask delay GET dl/S 6000 1 > /dev/null
    $U cat $f/slow.txt > /dev/null 2>&1 &
    c=$!
    sleep 1
    alive $c && echo "the opener waits; state meanwhile: $(ask state $f/slow.txt)" || echo "the opener did not wait"
    # Not SIGINT: a background command of a script ignores it.
    kill -TERM $c; sleep 1
    alive $c && echo "1 s after SIGTERM: still waiting" || echo "1 s after SIGTERM: gone"
    sleep 6
    echo "state once the download ended by itself: $(ask state $f/slow.txt)"
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
    # While nothing is mounted the user puts a file of their own where an item that is not
    # downloaded has its name.
    $U fusermount3 -u -z $f
    $U sh -c "mkdir -p $f/docs && echo 'the user own file' > $f/docs/big.bin"
    : > $W/logs/svc.out
    $U "$bin/fuse-service" $st < $st/in > $W/logs/svc.out 2> $W/logs/svc.err &
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
    echo "the user's own file after it all: $($U cat $f/docs/big.bin 2>&1)"
    exec 8>&-; wait $svc 2> /dev/null
    grep -v "^$" $W/logs/svc.err | tail -8
}

# The daemon on a private bus, `konedrivectl` and plain file calls on a folder served through
# FUSE: crates/konedrivectl/tests/sync_cli/fuse.rs.
cli() {
    echo "==== a folder served through FUSE, driven from the command line ===="
    ip link set lo up
    mkdir -p $W/tmp; chown 1000:1000 $W/tmp
    $U env TMPDIR=$W/tmp "$CLI_TEST" --ignored --exact fuse::a_folder_served_through_fuse_is_driven_from_the_command_line --nocapture 2>&1 | tail -25
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
