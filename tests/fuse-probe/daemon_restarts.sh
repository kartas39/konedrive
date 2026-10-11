# The scenarios of a daemon that is stopped or killed, is dead for a while and is started again, beyond
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

# The daemon stopped by SIGTERM, not killed, in the middle of the same upload: the stop ends
# by itself, and the file is sent once.
upload_session_cut_by_a_stop_goes_on_where_it_stood() {
    begin_folder
    register_folder
    onedrive throttle PUT upload/ 1 5 2 > /dev/null
    head -c 26214400 /dev/urandom > "$SRC/big"
    chown 1000:1000 "$SRC/big"
    user "cp '$SRC/big' big.bin"
    fragments_sent() { [ "$(onedrive count PUT upload/)" -ge 3 ]; }
    check_within 60 "the session is half sent" fragments_sent
    check "the service stops by itself" "$(stop_service)" Finished
    check "the mount is off the folder" "$(mounted "$F" && echo mounted || echo off)" off
    check "the folder itself holds the file, locked" "$($U cmp -s "$F/big.bin" "$SRC/big" && echo equal) $($U stat -c %a "$F" "$F/big.bin" | tr '\n' ' ')" "equal 555 444 "
    check_lacks "OneDrive does not hold the file whole yet" "$(onedrive compare big.bin "$SRC/big")" equal
    restart_service
    wait_for 90 onedrive_holds big.bin "$SRC/big"
    check "OneDrive holds the file whole" "$(onedrive compare big.bin "$SRC/big")" equal
    check_sent 30
    check "one upload session was opened, not two" "$(onedrive count POST createUploadSession)" 1
    check "OneDrive holds the one file" "$(onedrive paths)" big.bin
    check_state big.bin hydrated
}

# The daemon stopped by SIGTERM while a reader waits for a file that is being downloaded.
daemon_stopped_while_a_file_downloads_serves_it_whole_once_started_again() {
    begin_folder
    for n in 1 2 3 4; do head -c 2097152 /dev/urandom > "$SRC/seed-X$n"; onedrive file X$n R big$n.bin "$SRC/seed-X$n" > /dev/null; done
    seed_file A R small.txt 'small'
    register_folder read-only
    download small.txt
    onedrive delay GET dl/X 5000 16 > /dev/null
    mkdir "$SRC/got"
    chown 1000:1000 "$SRC/got"
    local readers=()
    for n in 1 2 3 4; do
        user "cat big$n.bin > '$SRC/got/$n' 2> /dev/null; echo \$? > '$SRC/got/$n.status'" &
        readers+=($!)
    done
    sleep 1
    check "the service stops by itself" "$(stop_service)" Finished
    readers_have_ended() { [ "$(ls "$SRC/got" | grep -c status)" = 4 ]; }
    check_within 5 "every reader is answered once the daemon is stopped" readers_have_ended
    kill "${readers[@]}" 2> /dev/null
    wait "${readers[@]}" 2> /dev/null
    local wrong=0
    for n in 1 2 3 4; do [ ! -s "$SRC/got/$n" ] || cmp -s "$SRC/got/$n" "$SRC/seed-X$n" || wrong=$((wrong + 1)); done
    check "no reader got bytes that are not the file's" "$wrong" 0
    check "the mount is off the folder" "$(mounted "$F" && echo mounted || echo off)" off
    check "the folder itself shows the downloaded file alone, locked" "$($U ls -A "$F" | tr '\n' ' ') $($U stat -c %a "$F" "$F/small.txt" | tr '\n' ' ')" "small.txt  555 444 "
    restart_service read-only
    check "a file whose download the stop cut is read whole" "$(user "cmp -s big2.bin '$SRC/seed-X2'" && echo equal)" equal
    check_state big2.bin hydrated
    check_state small.txt hydrated
}

# The daemon killed when everything was sent: the check at start finds the folder as the rows
# say, and nothing is sent a second time or lost.
daemon_killed_after_its_changes_were_sent_sends_nothing_again() {
    begin_folder
    seed_folder D R docs
    seed_file A R edited.txt 'the first version'
    seed_file B R renamed.txt 'renamed'
    seed_file C R removed.txt 'removed'
    seed_file E D moved.txt 'moved'
    seed_file G R online.txt 'never read'
    register_folder
    download edited.txt
    user "printf 'changed here' > edited.txt && mv renamed.txt renamed-here.txt && rm removed.txt && mv docs/moved.txt . && mkdir made && printf 'made here' > made/new.txt"
    check_onedrive_holds edited.txt 'changed here'
    check_onedrive_holds made/new.txt 'made here'
    check_within 30 "the rename is sent" onedrive_has renamed-here.txt
    check_within 30 "the removal is sent" onedrive_lacks removed.txt
    check_within 30 "the move is sent" onedrive_has moved.txt
    check_sent
    local sent shown
    sent=$(onedrive changes)
    shown=$(listing)
    kill_service
    tidy_folder
    restart_service
    daemon refresh > /dev/null
    sleep 3
    check "nothing waits to be uploaded" "$(daemon outbox)" ""
    check "nothing was sent a second time" "$(onedrive changes)" "$sent"
    check "OneDrive's bin holds the one removed file" "$(onedrive bin)" C
    check "the folder shows what it showed" "$(listing)" "$shown"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check_folder_holds edited.txt 'changed here'
    check_folder_holds made/new.txt 'made here'
    check "what was downloaded still is, and what was not is not" "$(state_of edited.txt) $(state_of made/new.txt) $(state_of online.txt)" "hydrated hydrated online-only"
    check_folder_holds online.txt 'never read'
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
    tidy_folder
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

# The group: every scenario of a daemon stopped or killed, and started again.
daemon_restarts() {
    echo "==== the daemon stopped or killed, and started again ===="
    ip link set lo up
    for name in \
        upload_session_cut_by_a_kill_goes_on_where_it_stood \
        upload_session_cut_by_a_stop_goes_on_where_it_stood \
        daemon_stopped_while_a_file_downloads_serves_it_whole_once_started_again \
        daemon_killed_after_its_changes_were_sent_sends_nothing_again \
        file_put_in_while_the_daemon_is_dead_is_uploaded_once_it_runs \
        file_edited_while_the_daemon_is_dead_is_uploaded_once_it_runs \
        readers_waiting_when_the_daemon_is_killed_get_an_error \
        file_that_is_not_downloaded_is_absent_while_the_daemon_is_dead \
        dead_mount_is_taken_off_at_once_after_a_kill \
        daemon_started_at_once_after_a_kill_serves_the_folder; do
        folder_scenario $name
    done
}
