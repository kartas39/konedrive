# The scenarios of reading while something goes wrong or goes on beside it: a download that
# fails, a folder forgotten, a free-up, a slow file, many readers, a full disk. In each, a
# reader gets the file's content or an error, never bytes that are not the file's. Sourced by
# guest.sh; each scenario has a read-only folder of its own (scenario_folder.sh).

# seed_random <id> <parent id> <name> <bytes>: a file of random bytes in OneDrive; what it
# holds stays in $SRC/seed-<id>.
seed_random() {
    head -c "$4" /dev/urandom > "$SRC/seed-$1"
    onedrive file "$1" "$2" "$3" "$SRC/seed-$1" > /dev/null
}

# is_seed <path> <id>: whether a read of the path gives what OneDrive holds of the item.
is_seed() { user "cmp -s '$1' '$SRC/seed-$2'"; }

# check_read_gives_the_file_or_fails <path> <id>: a read gives the file whole or an error, and
# after an error the file is shown as downloaded only if it is whole.
check_read_gives_the_file_or_fails() {
    if user "cat '$1' > '$SRC/got' 2> /dev/null"; then
        check "a read that went through gave the file whole" "$(cmp "$SRC/got" "$SRC/seed-$2" 2>&1 && echo equal)" equal
    else
        echo "    ok    the read was given an error"
        # The download that goes on behind the reads may have ended whole meanwhile, by its own tries: only a
        # file that is not whole must not be shown as downloaded.
        if [ "$(state_of "$1")" = hydrated ]; then
            check "after a read that failed, a file shown as downloaded is whole" "$(is_seed "$1" "$2" && echo equal)" equal
        else
            echo "    ok    after a read that failed, the file is not shown as downloaded"
        fi
    fi
}

download_cut_off_gives_no_wrong_bytes() {
    begin_folder
    seed_random X R big.bin 8388608
    register_folder read-only
    # The next downloads of the whole file end after 1 MiB.
    onedrive cut X 1048576 6 > /dev/null
    check_read_gives_the_file_or_fails big.bin X
    check "its size is still the file's" "$(user "stat -c %s big.bin")" 8388608
    check_holds "a download asked for by name does not end as if it were whole" "$(daemon hydrate "$F/big.bin"; is_seed big.bin X && echo ' equal')" equal
    for _ in 1 2 3 4 5 6; do user "cat big.bin > /dev/null 2>&1" && break; done
    check "a later read gives the file whole" "$(is_seed big.bin X && echo equal)" equal
    check_state big.bin hydrated
}

# A download asked for by name whose every try OneDrive ends after the first half: it fails,
# keeps what it has with the place it is checked up to, and the next asking goes on from
# there. (A file that is read, as `cat` reads it, is not cut by these answers: its reads ask
# for ranges, which the first half answers rightly.)
download_cut_off_keeps_what_it_has_and_goes_on_from_there() {
    begin_folder
    seed_random X R big.bin 41943040
    register_folder read-only
    # As many as the download tries before it gives up.
    onedrive cut X 20971520 3 > /dev/null
    check_holds "the download fails" "$(daemon hydrate "$F/big.bin")" "refused"
    check "the file is not shown as downloaded" "$(state_of big.bin)" online-only
    local left
    left=$(daemon staged)
    check_holds "the first half is kept, unfinished" "$left" "[(20971520, Some("
    check "OneDrive was asked three times" "$(onedrive count GET dl/X)" 3
    check "asked again, the download ends" "$(daemon hydrate "$F/big.bin")" downloaded
    check "with one more request" "$(onedrive count GET dl/X)" 4
    check "nothing is left unfinished" "$(daemon staged)" "[]"
    check "the file is whole" "$(is_seed big.bin X && echo equal)" equal
    check_state big.bin hydrated
}

# OneDrive changes the file while its first version is still being downloaded: what the
# folder shows afterwards is the version OneDrive holds now.
file_changed_in_onedrive_during_its_download_is_read_as_it_is_now() {
    begin_folder
    seed_random X R big.bin 4194304
    register_folder read-only
    onedrive delay GET dl/X 3000 1 > /dev/null
    user "cat big.bin > /dev/null 2>&1" &
    local reader=$!
    sleep 1
    cp "$SRC/seed-X" "$SRC/first"
    head -c 4194304 /dev/urandom > "$SRC/seed-X"
    onedrive edit X "$SRC/seed-X" > /dev/null
    daemon refresh > /dev/null
    wait $reader
    sleep 3
    check "a read gives the version OneDrive holds now" "$(is_seed big.bin X && echo equal || { user "cmp -s big.bin '$SRC/first'" && echo "the version before the change"; })" equal
    daemon refresh > /dev/null
    sleep 1
    check "and still does after a cycle" "$(is_seed big.bin X && echo equal || { user "cmp -s big.bin '$SRC/first'" && echo "the version before the change"; })" equal
}

download_in_flight_when_the_folder_is_forgotten_leaves_no_wrong_bytes() {
    begin_folder
    seed_random X R big.bin 4194304
    register_folder read-only
    onedrive delay GET dl/X 3000 1 > /dev/null
    user "cat big.bin > '$SRC/got' 2> /dev/null; echo \$? > '$SRC/got.status'" &
    local reader=$!
    sleep 1
    check "forgotten while the reader waits" "$(daemon forget)" forgotten
    wait $reader
    if [ "$(cat "$SRC/got.status")" = 0 ]; then
        check "the reader that got the file got it whole" "$(cmp -s "$SRC/got" "$SRC/seed-X" && echo equal)" equal
    else
        echo "    ok    the reader was given an error"
    fi
    check "nothing of the file is left in the folder itself" "$(user "ls -A" | tr '\n' ' ')" ""
    daemon register "$F" > /dev/null
    wait_for_folder read-only
    check "registered again, the file is not shown as downloaded" "$(state_of big.bin)" online-only
    check "and is read whole" "$(is_seed big.bin X && echo equal)" equal
}

free_up_while_a_reader_holds_the_file_gives_it_no_wrong_bytes() {
    begin_folder
    seed_random X R big.bin 2097152
    register_folder read-only
    download big.bin
    # A reader that stops in the middle of the file for two seconds.
    $U python3 -c "
import sys, time
with open(sys.argv[1], 'rb') as f:
    start = f.read(65536)
    time.sleep(2)
    rest = f.read()
open(sys.argv[2], 'wb').write(start + rest)" "$F/big.bin" "$SRC/got" &
    local reader=$!
    sleep 0.5
    echo "    note  the free-up while the file is held open: $(daemon dehydrate "$F/big.bin")"
    wait $reader
    check "the reader got the file whole" "$(cmp -s "$SRC/got" "$SRC/seed-X" && echo equal)" equal
    check "a read afterwards gives the file whole" "$(is_seed big.bin X && echo equal)" equal
}

slow_download_does_not_stall_another_file() {
    begin_folder
    seed_random X R slow.bin 1048576
    seed_random Y R other.bin 1048576
    register_folder read-only
    onedrive delay GET dl/X 6000 1 > /dev/null
    user "cat slow.bin > '$SRC/got'" &
    local reader=$! began ended
    sleep 1
    began=$(date +%s%N)
    check "the other file is read whole meanwhile" "$(is_seed other.bin Y && echo equal)" equal
    ended=$(date +%s%N)
    check "within three seconds" "$([ $(((ended - began) / 1000000)) -lt 3000 ] && echo yes || echo "no: $(((ended - began) / 1000000)) ms")" yes
    check "while the slow file still waits" "$(state_of slow.bin)" online-only
    wait $reader
    check "the slow file is read whole in the end" "$(cmp -s "$SRC/got" "$SRC/seed-X" && echo equal)" equal
}

many_files_read_at_once_all_get_their_content() {
    begin_folder
    seed_folder M R many
    for n in $(seq -w 1 200); do seed_file M$n M f$n.txt "the content of file $n, which no other file has"; done
    register_folder read-only
    mkdir "$SRC/got"
    chown 1000:1000 "$SRC/got"
    user "ls many | xargs -P 64 -I{} sh -c 'cat many/{} > $SRC/got/{} 2> /dev/null || echo {} >> $SRC/refused'"
    check "every reader was answered with the file" "$(cat "$SRC/refused" 2> /dev/null | wc -l)" 0
    local wrong=0
    for n in $(seq -w 1 200); do cmp -s "$SRC/got/f$n.txt" "$SRC/seed-M$n" || wrong=$((wrong + 1)); done
    check "no reader got bytes that are not the file's" "$wrong" 0
    check "all 200 were read" "$(ls "$SRC/got" | wc -l)" 200
}

download_onto_a_full_disk_fails_and_works_once_there_is_room() {
    begin_folder small-disk
    seed_random X R big.bin 8388608
    register_folder read-only
    # The disk filled to its last byte, and then 2 MiB given back: a download begins and
    # cannot end.
    user "dd if=/dev/zero of=../ballast bs=1M 2> /dev/null; dd if=/dev/zero of=../ballast bs=1 oflag=append conv=notrunc 2> /dev/null; sync; truncate -s -2M ../ballast; sync"
    check_read_gives_the_file_or_fails big.bin X
    check_holds "a download asked for by name is refused" "$(daemon hydrate "$F/big.bin")" "refused"
    check "the file is not shown as downloaded" "$(state_of big.bin)" online-only
    check "the folder still answers" "$(user "ls")" big.bin
    user "rm ../ballast; sync"
    check "with room again, the download goes through" "$(daemon hydrate "$F/big.bin")" downloaded
    check "and the file is read whole" "$(is_seed big.bin X && echo equal)" equal
    check_state big.bin hydrated
}

# The group: every scenario of reading beside a fault.
file_reads() {
    echo "==== reading beside a fault ===="
    ip link set lo up
    for name in \
        download_cut_off_gives_no_wrong_bytes \
        download_cut_off_keeps_what_it_has_and_goes_on_from_there \
        file_changed_in_onedrive_during_its_download_is_read_as_it_is_now \
        download_in_flight_when_the_folder_is_forgotten_leaves_no_wrong_bytes \
        free_up_while_a_reader_holds_the_file_gives_it_no_wrong_bytes \
        slow_download_does_not_stall_another_file \
        many_files_read_at_once_all_get_their_content \
        download_onto_a_full_disk_fails_and_works_once_there_is_room; do
        folder_scenario $name
    done
}
