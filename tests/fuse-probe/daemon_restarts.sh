# The scenarios of a daemon that is killed, is dead for a while and is started again, beyond
# the one of unsent changes (file_writes.sh). Sourced by guest.sh; each scenario has a folder
# of its own (scenario_folder.sh).

upload_session_cut_by_a_kill_goes_on_where_it_stood() {
    begin_folder
    register_folder
    # The second fragment of the session is throttled, both times the client asks: the
    # session is half sent when the daemon is killed.
    onedrive throttle PUT upload/ 1 5 2 > /dev/null
    head -c 26214400 /dev/urandom > "$SRC/big"
    chown 1000:1000 "$SRC/big"
    user "cp '$SRC/big' big.bin"
    fragments_sent() { [ "$(onedrive count PUT upload/)" -ge 3 ]; }
    check_within 60 "the session is half sent" fragments_sent
    kill_service
    tidy_folder
    check_lacks "OneDrive does not hold the file whole yet" "$(onedrive compare big.bin "$SRC/big")" equal
    restart_service
    wait_for 90 onedrive_holds big.bin "$SRC/big"
    check "OneDrive holds the file whole" "$(onedrive compare big.bin "$SRC/big")" equal
    check_sent 30
    check "one upload session was opened, not two" "$(onedrive count POST createUploadSession)" 1
    check "the session was asked where it stands" "$([ "$(onedrive count GET upload/)" -ge 1 ] && echo yes || echo no)" yes
    check "the folder shows the file whole" "$(user "cmp -s big.bin '$SRC/big'" && echo equal)" equal
    check_state big.bin hydrated
}

# Only on purpose: the daemon dead, the folder is locked, and the user unlocks it to put a
# file in.
file_put_in_while_the_daemon_is_dead_is_uploaded_once_it_runs() {
    begin_folder
    seed_folder D R docs
    seed_file A D a.txt 'a'
    register_folder
    download docs/a.txt
    kill_service
    tidy_folder
    check "the folder is locked, the daemon dead" "$($U sh -c "echo x > '$F/refused.txt'" 2>&1 | sed 's/.*: //')" "Permission denied"
    $U sh -c "chmod u+w '$F' '$F/docs' && printf 'put in while the daemon was dead' > '$F/docs/offline.txt' && mkdir '$F/offline' && printf 'inside' > '$F/offline/inside.txt'"
    restart_service
    check_folder_holds docs/offline.txt 'put in while the daemon was dead'
    check_onedrive_holds docs/offline.txt 'put in while the daemon was dead' 60
    check_onedrive_holds offline/inside.txt 'inside' 60
    check_sent
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
}

# Only on purpose, as above: the downloaded copy of a file changed in the folder itself.
file_edited_while_the_daemon_is_dead_is_uploaded_once_it_runs() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    kill_service
    tidy_folder
    $U sh -c "chmod u+w '$F/a.txt' && printf 'changed while the daemon was dead' > '$F/a.txt'"
    restart_service
    check_folder_holds a.txt 'changed while the daemon was dead'
    check_onedrive_holds a.txt 'changed while the daemon was dead' 60
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
    daemon refresh > /dev/null
    sleep 2
    check_folder_holds a.txt 'changed while the daemon was dead'
}

readers_waiting_when_the_daemon_is_killed_get_an_error() {
    begin_folder
    for n in 1 2 3 4 5 6 7 8; do seed_file X$n R f$n.txt "the content of file $n, long enough to tell from a run of zero bytes"; done
    register_folder read-only
    onedrive delay GET dl/X 30000 16 > /dev/null
    mkdir "$SRC/got"
    chown 1000:1000 "$SRC/got"
    local readers=()
    for n in 1 2 3 4 5 6 7 8; do
        user "cat f$n.txt > '$SRC/got/$n' 2> /dev/null; echo \$? > '$SRC/got/$n.status'" &
        readers+=($!)
    done
    sleep 1.5
    kill_service
    readers_have_ended() { [ "$(ls "$SRC/got" | grep -c status)" = 8 ]; }
    check_within 5 "every reader is answered once the daemon is dead" readers_have_ended
    kill "${readers[@]}" 2> /dev/null
    wait "${readers[@]}" 2> /dev/null
    check "each with an error" "$(cat "$SRC"/got/*.status | grep -vc '^0$')" 8
    check "none was given a byte" "$(cat "$SRC"/got/[1-8] | wc -c)" 0
    tidy_folder
    restart_service read-only
    check_folder_holds f3.txt 'the content of file 3, long enough to tell from a run of zero bytes'
    check_folder_holds f8.txt 'the content of file 8, long enough to tell from a run of zero bytes'
}

file_that_is_not_downloaded_is_absent_while_the_daemon_is_dead() {
    begin_folder
    seed_file A R downloaded.txt 'downloaded'
    seed_file B R online.txt 'online'
    register_folder read-only
    download downloaded.txt
    kill_service
    check "before the dead mount is taken off, the folder answers that it is not connected" "$($U cat "$F/online.txt" 2>&1 | sed 's/.*: //')" "Transport endpoint is not connected"
    tidy_folder
    check "the folder itself holds the downloaded file alone" "$($U ls -A "$F" | tr '\n' ' ')" "downloaded.txt "
    check "which is read" "$($U cat "$F/downloaded.txt")" downloaded
    check "the file that is not downloaded is not there" "$($U cat "$F/online.txt" 2>&1 | sed 's/.*: //')" "No such file or directory"
    check "the folder is locked" "$($U stat -c %a "$F" "$F/downloaded.txt" | tr '\n' ' ')" "555 444 "
    restart_service read-only
    check_folder_holds online.txt 'online'
    check_state downloaded.txt hydrated
}

# What the unit runs after the daemon's end, run as the unit runs it: at once.
dead_mount_is_taken_off_at_once_after_a_kill() {
    begin_folder
    seed_file A R downloaded.txt 'downloaded'
    register_folder read-only
    download downloaded.txt
    kill_service
    tidy_folder_at_once
    check "the dead mount is off the folder" "$(mounted "$F" && echo mounted || echo off)" off
    check "the folder itself shows the downloaded file" "$($U cat "$F/downloaded.txt" 2>&1 | sed 's/.*: //')" downloaded
}

# No tidy step in between, and no second waited: the daemon started again by hand.
daemon_started_at_once_after_a_kill_serves_the_folder() {
    begin_folder
    seed_file A R downloaded.txt 'downloaded'
    seed_file B R online.txt 'online'
    register_folder read-only
    download downloaded.txt
    kill_service
    start_service
    wait_for 10 first_cycle_is_over
    check_holds "the folder is up" "$(daemon status)" "state Ready"
    check_folder_holds downloaded.txt 'downloaded'
    check_folder_holds online.txt 'online'
}

# The group: every scenario of a daemon killed and started again.
daemon_restarts() {
    echo "==== the daemon killed and started again ===="
    ip link set lo up
    for name in \
        upload_session_cut_by_a_kill_goes_on_where_it_stood \
        file_put_in_while_the_daemon_is_dead_is_uploaded_once_it_runs \
        file_edited_while_the_daemon_is_dead_is_uploaded_once_it_runs \
        readers_waiting_when_the_daemon_is_killed_get_an_error \
        file_that_is_not_downloaded_is_absent_while_the_daemon_is_dead \
        dead_mount_is_taken_off_at_once_after_a_kill \
        daemon_started_at_once_after_a_kill_serves_the_folder; do
        folder_scenario $name
    done
}
