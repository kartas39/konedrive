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

# Stage 1: the read-only mount of crates/konedrive-fuse over a store seeded from a directory.
ro() {
    echo "==== stage 1: read-only mount over the folder, names from the store ===="
    local k="$KFUSE" c=$W/cloud f=$W/OneDrive st=$W/state/fuse-tree.sqlite p
    mkdir -p $c/docs/deep $f $W/state
    echo "hello from the cloud" > $c/a.txt
    echo "second" > $c/docs/b.txt
    head -c 1048576 /dev/urandom > $c/docs/deep/c.bin
    head -c 209715200 /dev/urandom > $c/big.bin
    echo "will be damaged" > $c/damaged.txt
    chown -R 1000:1000 $c $f $W/state
    $U "$k" seed $st $c
    echo "damaged in transit" > $c/damaged.txt
    kmount() {
        $U "$k" mount $f $st --local-dir $c 2> $W/logs/ro.err &
        p=$!
        for _ in $(seq 50); do mounted $f && return 0; sleep 0.1; done
        echo "MOUNT FAILED: $(cat $W/logs/ro.err)"; return 1
    }
    state() { $U python3 -c "import os,sys; print(os.getxattr(sys.argv[1],'user.konedrive.state').decode())" "$1"; }
    kmount || return
    echo "type: $(awk -v m=$f '$2==m{print $3}' /proc/self/mounts)"
    echo "listing: $($U find $f | sort | tr '\n' ' ')"
    echo "sizes: $($U stat -c '%n=%s/%b' $f/a.txt $f/big.bin | tr '\n' ' ')"
    echo "state of a.txt before: $(state $f/a.txt)"
    echo "a.txt: $($U cat $f/a.txt)"
    echo "state of a.txt after: $(state $f/a.txt)"
    $U cat $f/docs/deep/c.bin > /dev/null & $U cat $f/docs/deep/c.bin > /dev/null & wait $! 
    echo "c.bin equal to the cloud's: $($U cmp $f/docs/deep/c.bin $c/docs/deep/c.bin && echo yes)"
    s=$(date +%s.%N)
    echo "big.bin equal to the cloud's: $($U cmp $f/big.bin $c/big.bin && echo yes), first read took $(echo "$(date +%s.%N) - $s" | bc) s"
    echo "a file whose content does not match its hash: $($U cat $f/damaged.txt 2>&1)"
    echo "writes: $($U sh -c "touch $f/new; rm $f/a.txt; echo x >> $f/a.txt; mkdir $f/d; mv $f/a.txt $f/z" 2>&1 | sed 's/.*: //' | sort | uniq -c | tr '\n' ' ')"
    kill -KILL $p; wait $p 2> /dev/null
    echo "daemon killed: $($U ls $f 2>&1)"
    $U fusermount3 -u -z $f
    echo "the folder itself: $($U find $f -type f | sort | tr '\n' ' ')"
    echo "attributes of a downloaded file: $($U python3 -c "import os,sys; print({k: os.getxattr(sys.argv[1],k).decode() for k in os.listxattr(sys.argv[1])})" $f/a.txt)"
    mv $c $c.away
    kmount || return
    echo "mounted again, the cloud gone: a.txt: $($U cat $f/a.txt); b.txt: $($U cat $f/docs/b.txt 2>&1)"
    echo "states: a.txt $(state $f/a.txt), b.txt $(state $f/docs/b.txt)"
    $U fusermount3 -u $f; wait $p 2> /dev/null
}

# Stage 1 against the fake OneDrive: listed into the store, followed, announced.
graph() {
    echo "==== stage 1 with OneDrive: listing, download, and changes that arrive ===="
    local k="$KFUSE" f=$W/OneDrive2 st=$W/state2/fuse-tree.sqlite src=$W/src2 p cloud url
    ip link set lo up
    mkdir -p $f $W/state2 $src
    echo "hello" > $src/a; echo "to be deleted" > $src/gone; echo "keeps its content" > $src/keep
    echo "inside docs" > $src/b; head -c 52428800 /dev/urandom > $src/big; echo "hello, changed in the cloud" > $src/a2
    echo "arrived later" > $src/new; echo T > $W/state2/token
    chown -R 1000:1000 $f $W/state2 $src
    mkfifo $W/state2/in; chown 1000:1000 $W/state2/in
    $U "$bin/fake-cloud" < $W/state2/in > $W/logs/cloud.out 2> $W/logs/cloud.err &
    cloud=$!
    exec 9> $W/state2/in
    say() { echo "$*" >&9; for _ in $(seq 50); do grep -qF "done $*" $W/logs/cloud.out && return; sleep 0.1; done; echo "the fake cloud did not answer: $*"; }
    for _ in $(seq 50); do url=$(awk '$1=="url"{print $2}' $W/logs/cloud.out); [ -n "$url" ] && break; sleep 0.1; done
    say file A R a.txt $src/a; say file G R gone.txt $src/gone; say file K R keep.txt $src/keep
    say folder D R docs; say file B D b.txt $src/b; say file X D big.bin $src/big
    $U "$k" mount $f $st --graph "$url" --token-file $W/state2/token --poll-secs 1 2> $W/logs/graph.err &
    p=$!
    for _ in $(seq 100); do mounted $f && break; sleep 0.1; done
    mounted $f || { echo "MOUNT FAILED: $(cat $W/logs/graph.err)"; return; }
    state() { $U python3 -c "import os,sys; print(os.getxattr(sys.argv[1],'user.konedrive.state').decode())" "$1"; }
    echo "listing: $($U find $f | sort | tr '\n' ' ')"
    echo "a.txt: $($U cat $f/a.txt); keep.txt: $($U cat $f/keep.txt); gone.txt: $($U cat $f/gone.txt)"
    echo "big.bin equal to the cloud's: $($U cmp $f/docs/big.bin $src/big && echo yes)"
    $U "$bin/probe-client" watch $f 9 > $W/logs/watch.out &
    w=$!
    sleep 1
    say file N R new.txt $src/new; sleep 2
    say edit A $src/a2; sleep 2
    say trash G; sleep 2
    say rename K R moved.txt
    wait $w
    echo "an inotify watch on the folder saw: $(tr '\n' ';' < $W/logs/watch.out)"
    echo "listing: $($U ls $f | tr '\n' ' ')"
    echo "state of a.txt after the change: $(state $f/a.txt); a.txt: $($U cat $f/a.txt)"
    echo "state of moved.txt: $(state $f/moved.txt); moved.txt: $($U cat $f/moved.txt); new.txt: $($U cat $f/new.txt)"
    $U fusermount3 -u $f; wait $p 2> /dev/null
    echo "the folder itself: $($U find $f -type f | sort | tr '\n' ' ')"
    cat $W/logs/graph.err
    exec 9>&-; wait $cloud 2> /dev/null
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
