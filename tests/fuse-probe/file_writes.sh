# The scenarios of writing at the level of files: what a person or a program does to a
# read-write folder, and what OneDrive and the folder hold afterwards. Sourced by guest.sh;
# each scenario has a folder of its own (scenario_folder.sh).

# -- a new file, a new directory

new_file_arrives_in_onedrive() {
    begin_folder
    register_folder
    user "printf 'made here' > new.txt"
    check "a file that is not in OneDrive yet has no state" "$(state_of new.txt)" none
    check_onedrive_holds new.txt 'made here'
    check_sent
    check_folder_holds new.txt 'made here'
    check_state new.txt hydrated
    check "the folder" "$(listing)" "new.txt"
}

new_directory_with_files_arrives_in_onedrive() {
    begin_folder
    register_folder
    user "mkdir -p album/inner && printf 'one' > album/one.txt && printf 'two' > album/inner/two.txt && mkdir album/empty"
    check_onedrive_holds album/one.txt 'one'
    check_onedrive_holds album/inner/two.txt 'two'
    check_within 20 "OneDrive has the empty directory" onedrive_has album/empty
    check_sent
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check_folder_holds album/inner/two.txt 'two'
    check_state album/one.txt hydrated
}

# -- an edit

edit_of_downloaded_file_is_uploaded() {
    begin_folder
    seed_file A R a.txt 'first version'
    register_folder
    download a.txt
    user "printf ', and a line added here' >> a.txt"
    check_onedrive_holds a.txt 'first version, and a line added here'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
    check_folder_holds a.txt 'first version, and a line added here'
    check_state a.txt hydrated
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
}

edit_of_file_that_is_not_downloaded_downloads_it_first() {
    begin_folder
    seed_file A R a.txt 'the version of OneDrive'
    register_folder
    check "not downloaded before the edit" "$(state_of a.txt)" online-only
    user "printf ', and a line added here' >> a.txt"
    check_folder_holds a.txt 'the version of OneDrive, and a line added here'
    check_onedrive_holds a.txt 'the version of OneDrive, and a line added here'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
    check_state a.txt hydrated
    check "downloaded once" "$(onedrive count GET dl/A)" 1
}

overwrite_in_place_of_file_that_is_not_downloaded_keeps_the_rest() {
    begin_folder
    seed_file A R a.txt '0123456789 the rest of the file stays'
    register_folder
    errno_of "f = os.open('a.txt', os.O_WRONLY); os.pwrite(f, b'HERE', 3); os.close(f)" > /dev/null
    check_folder_holds a.txt '012HERE789 the rest of the file stays'
    check_onedrive_holds a.txt '012HERE789 the rest of the file stays'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
}

# -- a removal

removed_files_go_to_onedrives_bin() {
    begin_folder
    seed_file A R downloaded.txt 'read before it was removed'
    seed_file B R online.txt 'never read'
    seed_file C R stays.txt 'stays'
    register_folder
    download downloaded.txt
    user "rm downloaded.txt online.txt"
    check_within 30 "OneDrive lacks the downloaded file" onedrive_lacks downloaded.txt
    check_within 30 "OneDrive lacks the file that was not downloaded" onedrive_lacks online.txt
    check_sent
    check "OneDrive's bin" "$(onedrive bin)" "A B"
    daemon refresh > /dev/null
    sleep 1
    check "the folder, after a cycle" "$(listing)" "stays.txt"
    check "nothing of the removed file is left on disk" "$(user "ls -A" | tr '\n' ' ')" "stays.txt "
}

removed_directory_goes_from_onedrive() {
    begin_folder
    seed_folder D R docs
    seed_folder S D sub
    seed_file A D a.txt 'a'
    seed_file B S b.txt 'b'
    for n in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do seed_file K$n R keep$n.txt 'kept'; done
    register_folder
    download docs/a.txt
    user "rm -r docs"
    check_within 30 "OneDrive lacks the directory" onedrive_lacks docs
    check_sent
    check_holds "OneDrive's bin holds the directory" "$(onedrive bin)" D
    daemon refresh > /dev/null
    sleep 1
    check "the folder lacks the directory, after a cycle" "$(user "test -e docs && echo there || echo gone")" gone
    check "the other files are still in OneDrive" "$(onedrive id keep7.txt)" K7
}

# A folder of 30 files in a drive of 37 items: a removal of 31, most of what the folder shows.
seed_large_directory() {
    seed_folder M R many
    for n in $(seq -w 1 30); do seed_file M$n M f$n.txt "file $n"; done
    for n in 1 2 3 4 5; do seed_file K$n R keep$n.txt 'kept'; done
}

# A removal made here is sent as any other change, however much it removes: nothing is held
# and nothing is asked. What protects the user is OneDrive's own recycle bin.
removing_a_large_directory_sends_every_removal() {
    begin_folder
    seed_large_directory
    register_folder
    user "rm -r many"
    check "the folder lacks the directory at once" "$(user "test -e many && echo there || echo gone")" gone
    check_within 30 "OneDrive lacks the directory" onedrive_lacks many
    check_sent
    check "the queue has no entry left, and asks nothing" "$(daemon upload-queue)" ""
    check "nothing is pending or blocked" "$(daemon upload-queue-status | cut -d, -f1,2)" "pending 0, blocked 0"
    check_holds "OneDrive's bin holds the directory" "$(onedrive bin)" "M "
    check_holds "and the first of its files" "$(onedrive bin)" M01
    check_holds "and the last of them" "$(onedrive bin)" M30
    check "all thirty-one are in the bin" "$(onedrive bin | wc -w)" 31
    check "the other files are still in OneDrive" "$(onedrive id keep3.txt)" K3
    check "and none of them is in the bin" "$(onedrive bin | tr ' ' '\n' | grep -c '^K')" 0
}

# -- a rename, a move

renamed_file_keeps_its_item() {
    begin_folder
    seed_file A R downloaded.txt 'downloaded'
    seed_file B R online.txt 'online'
    register_folder
    download downloaded.txt
    user "mv downloaded.txt downloaded-renamed.txt && mv online.txt online-renamed.txt"
    check_within 30 "OneDrive has the downloaded file at its new name" onedrive_has downloaded-renamed.txt
    check_within 30 "OneDrive has the file that is not downloaded at its new name" onedrive_has online-renamed.txt
    check_sent
    check "the items are the same" "$(onedrive id downloaded-renamed.txt) $(onedrive id online-renamed.txt)" "A B"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
    check_folder_holds downloaded-renamed.txt 'downloaded'
    check_state downloaded-renamed.txt hydrated
    check_state online-renamed.txt online-only
    check_folder_holds online-renamed.txt 'online'
}

moved_file_keeps_its_item() {
    begin_folder
    seed_folder D R docs
    seed_file A R downloaded.txt 'downloaded'
    seed_file B R online.txt 'online'
    seed_file C D inside.txt 'inside'
    register_folder
    download downloaded.txt
    user "mv downloaded.txt online.txt docs/ && mv docs/inside.txt outside-renamed.txt"
    check_within 30 "OneDrive has the downloaded file in the directory" onedrive_has docs/downloaded.txt
    check_within 30 "OneDrive has the file that is not downloaded in the directory" onedrive_has docs/online.txt
    check_within 30 "OneDrive has the file moved out of the directory under its new name" onedrive_has outside-renamed.txt
    check_sent
    check "the items are the same" "$(onedrive id docs/downloaded.txt) $(onedrive id docs/online.txt) $(onedrive id outside-renamed.txt)" "A B C"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check_folder_holds docs/downloaded.txt 'downloaded'
    check_folder_holds docs/online.txt 'online'
    check_folder_holds outside-renamed.txt 'inside'
}

renamed_and_moved_directory_keeps_its_items() {
    begin_folder
    seed_folder D R docs
    seed_folder S D sub
    seed_folder T R archive
    seed_file A D a.txt 'a'
    seed_file B S b.txt 'b'
    register_folder
    download docs/a.txt
    user "mv docs papers"
    check_within 30 "OneDrive has the directory at its new name" onedrive_has papers/sub/b.txt
    check_sent
    check "the items are the same" "$(onedrive id papers) $(onedrive id papers/a.txt) $(onedrive id papers/sub/b.txt)" "D A B"
    user "mv papers archive/"
    check_within 30 "OneDrive has the directory in the other one" onedrive_has archive/papers/sub/b.txt
    check_sent
    check "the items are the same after the move" "$(onedrive id archive/papers) $(onedrive id archive/papers/a.txt) $(onedrive id archive/papers/sub/b.txt)" "D A B"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
    check_folder_holds archive/papers/a.txt 'a'
    check_folder_holds archive/papers/sub/b.txt 'b'
    check_state archive/papers/a.txt hydrated
}

# -- a save by rename, as editors make it

# check_one_update_of <path> <id> <text>: the file was updated as the item it was, and nothing
# was removed from OneDrive or made in it.
check_one_update_of() {
    check_onedrive_holds "$1" "$3"
    check_sent
    check "the same item of OneDrive was updated" "$(onedrive id "$1")" "$2"
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
    check "nothing was removed from OneDrive" "$(onedrive count DELETE items)" 0
    check "OneDrive holds the one file" "$(onedrive paths)" "$1"
    check "the folder holds the one file" "$(listing)" "$1"
    check_folder_holds "$1" "$3"
    check_state "$1" hydrated
}

save_by_rename_over_downloaded_file_is_one_update() {
    begin_folder
    seed_file A R report.txt 'the first draft'
    register_folder
    download report.txt
    user "printf 'the second draft' > report.tmp && mv report.tmp report.txt"
    check_one_update_of report.txt A 'the second draft'
}

save_by_rename_over_file_that_is_not_downloaded_is_one_update() {
    begin_folder
    seed_file A R report.txt 'the first draft'
    register_folder
    user "printf 'the second draft' > report.tmp && mv report.tmp report.txt"
    check_one_update_of report.txt A 'the second draft'
    check "the version that was replaced was not downloaded" "$(onedrive count GET dl/A)" 0
}

# The temporary name is one the ignore list does not hold: the file is one that would be
# uploaded, had it stayed.
save_by_rename_from_a_name_that_is_not_ignored_is_one_update() {
    begin_folder
    seed_file A R report.txt 'the first draft'
    register_folder
    download report.txt
    user "printf 'the second draft' > report.txt.new && mv report.txt.new report.txt"
    check_one_update_of report.txt A 'the second draft'
}

# -- a truncation

truncated_downloaded_file_is_uploaded_shorter() {
    begin_folder
    seed_file A R a.txt '0123456789'
    register_folder
    download a.txt
    user "truncate -s 4 a.txt"
    check_folder_holds a.txt '0123'
    check_onedrive_holds a.txt '0123'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
}

truncated_file_that_is_not_downloaded_keeps_its_start() {
    begin_folder
    seed_file A R a.txt '0123456789'
    register_folder
    user "truncate -s 4 a.txt"
    check_folder_holds a.txt '0123'
    check_onedrive_holds a.txt '0123'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
}

file_truncated_to_nothing_is_uploaded_empty() {
    begin_folder
    seed_file A R downloaded.txt '0123456789'
    seed_file B R online.txt '0123456789'
    register_folder
    download downloaded.txt
    user "truncate -s 0 downloaded.txt && truncate -s 0 online.txt"
    check "the downloaded file is empty here" "$(user "stat -c %s downloaded.txt")" 0
    check "the file that was not downloaded is empty here" "$(user "stat -c %s online.txt")" 0
    check_onedrive_holds downloaded.txt ''
    check_onedrive_holds online.txt ''
    check "the same items of OneDrive were updated" "$(onedrive id downloaded.txt) $(onedrive id online.txt)" "A B"
    check_sent
}

file_opened_with_o_trunc_and_written_is_uploaded() {
    begin_folder
    seed_file A R downloaded.txt 'the version of OneDrive, which is longer'
    seed_file B R online.txt 'the version of OneDrive, which is longer'
    register_folder
    download downloaded.txt
    user "printf 'written anew' > downloaded.txt && printf 'written anew' > online.txt"
    check_folder_holds downloaded.txt 'written anew'
    check_folder_holds online.txt 'written anew'
    check_onedrive_holds downloaded.txt 'written anew'
    check_onedrive_holds online.txt 'written anew'
    check "the same items of OneDrive were updated" "$(onedrive id downloaded.txt) $(onedrive id online.txt)" "A B"
    check_sent
    check_state online.txt hydrated
    check "the version that was replaced was not downloaded" "$(onedrive count GET dl/B)" 0
}

file_opened_with_o_trunc_and_closed_is_uploaded_empty() {
    begin_folder
    seed_file A R downloaded.txt 'the version of OneDrive'
    seed_file B R online.txt 'the version of OneDrive'
    register_folder
    download downloaded.txt
    user ": > downloaded.txt && : > online.txt"
    check "the downloaded file is empty here" "$(user "stat -c %s downloaded.txt")" 0
    check "the file that was not downloaded is empty here" "$(user "stat -c %s online.txt")" 0
    check_onedrive_holds downloaded.txt ''
    check_onedrive_holds online.txt ''
    check "the same items of OneDrive were updated" "$(onedrive id downloaded.txt) $(onedrive id online.txt)" "A B"
    check_sent
}

# -- changed here and in OneDrive

# check_both_versions_kept <stem> <extension> <OneDrive's text> <this computer's text>
check_both_versions_kept() {
    local copy
    wait_for 30 user "ls | grep -q '^$1-.*$2\$'"
    copy=$(user "ls | grep '^$1-.*$2\$'" | head -1)
    check_holds "this computer's version is kept beside the file, under a conflict copy's name" "$copy" "$1-"
    check_folder_holds "$copy" "$4"
    check_onedrive_holds "$copy" "$4"
    check_onedrive_holds "$1$2" "$3"
    wait_for 10 user "grep -qx '$3' '$1$2'"
    check_folder_holds "$1$2" "$3"
    check_sent
    check_holds "the conflict is listed" "$(daemon conflicts)" "$copy"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
}

# OneDrive's change is not known here when the file is changed: the upload finds it.
file_changed_in_onedrive_before_the_upload_keeps_both() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    # The cycle that would bring OneDrive's change waits for its answer.
    onedrive delay GET delta 4000 3 > /dev/null
    printf "the version of OneDrive" > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    user "printf 'the version of this computer' > a.txt"
    check_both_versions_kept a .txt "the version of OneDrive" 'the version of this computer'
}

# The change made here is not sent yet when a cycle brings OneDrive's: the cycle finds it.
file_changed_in_onedrive_while_a_change_waits_keeps_both() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "printf 'the version of this computer' > a.txt"
    wait_for 10 upload_queue_has_rows
    printf "the version of OneDrive" > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    daemon refresh > /dev/null
    sleep 2
    check_holds "this computer's version is still in the folder" "$(user "cat *")" 'the version of this computer'
    onedrive outage off > /dev/null
    check_both_versions_kept a .txt "the version of OneDrive" 'the version of this computer'
}

file_removed_in_onedrive_while_changed_here_is_uploaded_again() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "printf 'the version of this computer' > a.txt"
    wait_for 10 upload_queue_has_rows
    onedrive trash A > /dev/null
    daemon refresh > /dev/null
    sleep 2
    check_folder_holds a.txt 'the version of this computer'
    onedrive outage off > /dev/null
    check_onedrive_holds a.txt 'the version of this computer'
    check_sent
    check_folder_holds a.txt 'the version of this computer'
}

file_changed_in_onedrive_while_removed_here_comes_back() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "rm a.txt"
    wait_for 10 upload_queue_has_rows
    printf "the version of OneDrive" > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    onedrive outage off > /dev/null
    daemon refresh > /dev/null
    check_within 30 "the file is in the folder again" folder_has a.txt
    check_folder_holds a.txt "the version of OneDrive"
    check_sent
    check "OneDrive still holds its version" "$(onedrive compare a.txt "$SRC/theirs")" equal
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
}

# -- read-only and back

# Every change a program can ask for, each with the errno it ends with.
every_change_errno() {
    echo "create $(errno_of "os.close(os.open('made.txt', os.O_WRONLY | os.O_CREAT, 0o644))")"
    echo "mkdir $(errno_of "os.mkdir('made')")"
    echo "write $(errno_of "os.close(os.open('a.txt', os.O_WRONLY))")"
    echo "o_trunc $(errno_of "os.close(os.open('a.txt', os.O_WRONLY | os.O_TRUNC))")"
    echo "truncate $(errno_of "os.truncate('a.txt', 2)")"
    echo "rename $(errno_of "os.rename('a.txt', 'b.txt')")"
    echo "move $(errno_of "os.rename('a.txt', 'docs/a.txt')")"
    echo "unlink $(errno_of "os.unlink('docs/online.txt')")"
    echo "rmdir $(errno_of "os.rmdir('empty')")"
    echo "utime $(errno_of "os.utime('a.txt', (1, 1))")"
}

read_only_folder_refuses_every_change() {
    begin_folder
    seed_folder D R docs
    seed_folder E R empty
    seed_file A R a.txt 'the version of OneDrive'
    seed_file B D online.txt 'online'
    register_folder
    download a.txt
    check "read-only" "$(daemon mode read-only)" read-only
    modes_are() { [ "$(user "stat -c %a . docs a.txt docs/online.txt" | tr '\n' ' ')" = "$1" ]; }
    wait_for 5 modes_are "555 555 444 444 "
    check "the modes of the folder, a directory and a file show no write bit" "$(user "stat -c %a . docs a.txt docs/online.txt" | tr '\n' ' ')" "555 555 444 444 "
    check "every change is refused as on a read-only filesystem" "$(every_change_errno | tr '\n' ' ')" \
        "create EROFS mkdir EROFS write EROFS o_trunc EROFS truncate EROFS rename EROFS move EROFS unlink EROFS rmdir EROFS utime EROFS "
    check_folder_holds a.txt 'the version of OneDrive'
    check_folder_holds docs/online.txt 'online'
    check "the folder is as it was" "$(listing)" "a.txt docs docs/online.txt empty"
    check "nothing waits to be uploaded" "$(daemon upload-queue)" ""
    check "nothing was changed in OneDrive" "$(onedrive changes)" 0
    check "read-write again" "$(daemon mode read-write)" read-write
    wait_for 5 modes_are "755 755 644 644 "
    check "the modes show the write bit again" "$(user "stat -c %a . docs a.txt docs/online.txt" | tr '\n' ' ')" "755 755 644 644 "
    user "printf 'changed after the switch back' > a.txt && printf 'made after the switch back' > docs/new.txt"
    check_onedrive_holds a.txt 'changed after the switch back'
    check_onedrive_holds docs/new.txt 'made after the switch back'
    check_sent
}

# The account turned read-only by something other than the user's switch (a sign-out, a
# narrower grant): the changes that were not sent wait, and go once it is read-write again.
unsent_changes_wait_while_the_folder_is_read_only() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "printf 'changed here' > a.txt && printf 'made here' > new.txt && mkdir made && printf 'inside' > made/inside.txt"
    wait_for 10 upload_queue_has_rows
    check "read-only, the changes kept" "$(daemon mode read-only keep)" read-only
    check "a change is refused" "$(errno_of "os.close(os.open('third.txt', os.O_WRONLY | os.O_CREAT, 0o644))")" EROFS
    check "a write to a file with an unsent change is refused" "$(errno_of "os.close(os.open('new.txt', os.O_WRONLY))")" EROFS
    check_holds "the changes still wait" "$(daemon upload-queue)" "new.txt"
    onedrive outage off > /dev/null
    daemon refresh > /dev/null
    sleep 3
    check_folder_holds a.txt 'changed here'
    check_folder_holds new.txt 'made here'
    check_folder_holds made/inside.txt 'inside'
    check "nothing was sent while read-only" "$(onedrive changes)" 0
    check "OneDrive still holds its version" "$(onedrive compare a.txt "$SRC/seed-A")" equal
    check "read-write again" "$(daemon mode read-write)" read-write
    check_onedrive_holds a.txt 'changed here' 60
    check_onedrive_holds new.txt 'made here' 60
    check_onedrive_holds made/inside.txt 'inside' 60
    check_sent
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
}

# The user's forced switch (the owner's decision of 2026-10-11 on a switch to read-only with
# unsent changes): it goes through at once, the changes that were not sent stay in the folder,
# are not sent while it is read-only, and go once it is read-write again. Nothing is dropped.
forced_switch_to_read_only_keeps_unsent_changes_and_sends_them_once_read_write() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "printf 'changed here' > a.txt && printf 'made here' > new.txt"
    wait_for 10 upload_queue_has_rows
    check "read-only, forced" "$(daemon mode read-only)" read-only
    check "a change is refused" "$(errno_of "os.close(os.open('third.txt', os.O_WRONLY | os.O_CREAT, 0o644))")" EROFS
    onedrive outage off > /dev/null
    daemon refresh > /dev/null
    sleep 3
    check "nothing was sent" "$(onedrive changes)" 0
    check_holds "the new file still waits" "$(daemon upload-queue)" "new.txt"
    check_holds "the changed file still waits" "$(daemon upload-queue)" "a.txt"
    check_holds "and the queue says why" "$(daemon upload-queue)" "the folder is read-only"
    check_folder_holds new.txt 'made here'
    check_folder_holds a.txt 'changed here'
    check "read-write again" "$(daemon mode read-write)" read-write
    check_onedrive_holds new.txt 'made here'
    check_onedrive_holds a.txt 'changed here'
    check "the same item of OneDrive was updated" "$(onedrive id a.txt)" A
    check_sent
    check_folder_holds new.txt 'made here'
    check_folder_holds a.txt 'changed here'
}

# A program holds a file open for writing when the switch is forced, and OneDrive changes the
# file meanwhile: what the program wrote is not lost to the cycle that brings the change.
file_held_open_for_writing_across_a_forced_switch_to_read_only_keeps_what_was_written() {
    begin_folder
    seed_file A R a.txt 'the first version'
    register_folder
    download a.txt
    $U python3 -c "
import os, sys, time
f = os.open(sys.argv[1], os.O_WRONLY | os.O_TRUNC)
os.write(f, b'written before the switch')
time.sleep(6)
os.close(f)" "$F/a.txt" &
    local writer=$!
    sleep 1
    check "read-only, forced" "$(daemon mode read-only)" read-only
    printf 'the version of OneDrive' > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    daemon refresh > /dev/null
    sleep 2
    check_holds "what the program wrote is in the folder while it holds the file" "$(user "cat *")" 'written before the switch'
    wait $writer
    daemon refresh > /dev/null
    sleep 2
    check_holds "what the program wrote is in the folder after it closed the file" "$(user "cat *")" 'written before the switch'
    check "nothing was changed in OneDrive" "$(onedrive changes)" 0
}

# -- pins, and free space

pinned_file_stays_downloaded() {
    begin_folder
    seed_file A R a.txt 'pinned'
    register_folder
    check "pinned" "$(daemon pin "$F/a.txt")" "1 pinned"
    check_state a.txt hydrated
    check_holds "a pinned file is not freed" "$(daemon dehydrate "$F/a.txt")" "refused"
    check_holds "free-up-space leaves it" "$(daemon free-up-space)" "files: 0"
    check_state a.txt hydrated
    user "printf 'pinned, and changed here' > a.txt"
    check_onedrive_holds a.txt 'pinned, and changed here'
    check_sent
    printf 'pinned, and changed in OneDrive' > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    daemon refresh > /dev/null
    wait_for 10 user "grep -q 'in OneDrive' a.txt"
    check_state a.txt hydrated
    check_folder_holds a.txt 'pinned, and changed in OneDrive'
    check "unpinned" "$(daemon unpin "$F/a.txt")" "1 unpinned"
    check "freed once it is unpinned" "$(daemon dehydrate "$F/a.txt")" freed
    check_state a.txt online-only
}

pinned_directory_keeps_what_is_in_it_downloaded() {
    begin_folder
    seed_folder D R docs
    seed_folder S D sub
    seed_file A D a.txt 'a'
    seed_file B S b.txt 'b'
    seed_file C R outside.txt 'outside'
    register_folder
    check "pinned" "$(daemon pin "$F/docs")" "1 pinned"
    check_state docs/a.txt hydrated
    check_state docs/sub/b.txt hydrated
    check "a file outside the directory is not downloaded" "$(state_of outside.txt)" online-only
    user "printf 'made in the pinned directory' > docs/made.txt && mv outside.txt docs/"
    check_onedrive_holds docs/made.txt 'made in the pinned directory'
    check_within 20 "OneDrive has the file moved in" onedrive_has docs/outside.txt
    check_sent
    check_state docs/made.txt hydrated
    check_state docs/outside.txt hydrated
    seed_file N D later.txt 'arrived later'
    daemon refresh > /dev/null
    check_state docs/later.txt hydrated
    check_holds "a file under the pin is not freed" "$(daemon dehydrate "$F/docs/a.txt")" "refused"
    check_holds "one file under the pin is freed up on its own" "$(daemon free-up "$F/docs/sub/b.txt")" "files: 1"
    check_state docs/sub/b.txt online-only
    seed_file M D after.txt 'arrived after the free-up'
    daemon refresh > /dev/null
    check_state docs/after.txt hydrated
    check "the pin leaves the file that was freed up alone" "$(state_of docs/sub/b.txt)" online-only
    check_state docs/a.txt hydrated
    check "kept again" "$(daemon pin "$F/docs/sub/b.txt")" "1 pinned"
    check_state docs/sub/b.txt hydrated
    check "unpinned" "$(daemon unpin "$F/docs")" "1 unpinned"
    check_holds "freed once the directory is unpinned" "$(daemon free-up "$F/docs")" "files: 6"
    check_state docs/a.txt online-only
    check_state docs/made.txt online-only
    check_folder_holds docs/made.txt 'made in the pinned directory'
}

free_up_keeps_unsent_changes() {
    begin_folder
    seed_folder D R docs
    seed_file A D sent.txt 'downloaded, not changed'
    seed_file B D changed.txt 'the first version'
    register_folder
    download docs/sent.txt
    download docs/changed.txt
    onedrive outage on > /dev/null
    user "printf 'changed here, not sent' > docs/changed.txt && printf 'made here, not sent' > docs/new.txt"
    wait_for 10 upload_queue_has_rows
    check_holds "a file with an unsent change is not freed by itself" "$(daemon dehydrate "$F/docs/changed.txt")" "refused"
    check_holds "freeing the directory frees the one file that is sent" "$(daemon free-up "$F/docs")" "files: 1,"
    check_state docs/sent.txt online-only
    check_folder_holds docs/changed.txt 'changed here, not sent'
    check_folder_holds docs/new.txt 'made here, not sent'
    download docs/sent.txt
    check_holds "free-up-space frees the one file that is sent" "$(daemon free-up-space)" "files: 1,"
    check_state docs/sent.txt online-only
    check_folder_holds docs/changed.txt 'changed here, not sent'
    check_folder_holds docs/new.txt 'made here, not sent'
    check_lacks "the changes still wait" "$(daemon upload-queue-status)" "pending 0,"
    onedrive outage off > /dev/null
    check_onedrive_holds docs/changed.txt 'changed here, not sent' 90
    check_onedrive_holds docs/new.txt 'made here, not sent' 90
    check_sent
}

# -- the daemon killed

daemon_killed_with_unsent_changes_sends_them_once_started_again() {
    begin_folder
    seed_folder D R docs
    seed_file A R edited.txt 'the first version'
    seed_file B R renamed.txt 'renamed'
    seed_file C R removed.txt 'removed'
    seed_file E D moved.txt 'moved'
    register_folder
    download edited.txt
    onedrive outage on > /dev/null
    user "printf 'changed here' > edited.txt && mv renamed.txt renamed-here.txt && rm removed.txt && mv docs/moved.txt . && mkdir made && printf 'made here' > made/new.txt"
    wait_for 10 upload_queue_has_rows
    check_lacks "the changes wait" "$(daemon upload-queue-status)" "pending 0,"
    kill_service
    tidy_folder
    check "the folder itself holds the content changed here, the daemon dead" "$($U cat "$F/edited.txt" "$F/made/new.txt")" 'changed heremade here'
    check "nothing was changed in OneDrive" "$(onedrive paths)" "docs docs/moved.txt edited.txt removed.txt renamed.txt"
    onedrive outage off > /dev/null
    restart_service
    check_onedrive_holds edited.txt 'changed here' 90
    check_onedrive_holds made/new.txt 'made here' 90
    check_within 90 "the rename is sent" onedrive_has renamed-here.txt
    check_within 90 "the removal is sent" onedrive_lacks removed.txt
    check_within 90 "the move is sent" onedrive_has moved.txt
    check_sent 60
    check "the items are the same" "$(onedrive id edited.txt) $(onedrive id renamed-here.txt) $(onedrive id moved.txt)" "A B E"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check_folder_holds edited.txt 'changed here'
    check_folder_holds made/new.txt 'made here'
}

# -- into the folder, out of it, and all of it in a row

file_that_is_not_downloaded_moved_into_new_directory_is_read_there() {
    begin_folder
    seed_file P R p.txt 'not downloaded when it was moved'
    register_folder
    user "mkdir fresh && mv p.txt fresh/"
    check "still not downloaded after the move" "$(state_of fresh/p.txt)" online-only
    check_folder_holds fresh/p.txt 'not downloaded when it was moved'
    check_within 30 "OneDrive has the file in the new directory" onedrive_has fresh/p.txt
    check_sent
    check "the item is the same" "$(onedrive id fresh/p.txt)" P
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check "nothing went to OneDrive's bin" "$(onedrive bin)" ""
}

# The tree comes from another directory of the same disk: across the mount, so `mv` copies it.
tree_moved_in_from_outside_is_uploaded() {
    begin_folder
    register_folder
    $U sh -c "mkdir -p '$WORLD/t/a/b' && printf 'TOP' > '$WORLD/t/top.txt' && printf 'LEAF' > '$WORLD/t/a/b/leaf.txt' && mv '$WORLD/t' '$F/t'"
    check_onedrive_holds t/top.txt 'TOP'
    check_onedrive_holds t/a/b/leaf.txt 'LEAF'
    check_sent
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    check "the tree is not outside any more" "$($U sh -c "test -e '$WORLD/t' && echo there || echo gone")" gone
}

# The file goes to another directory of the same disk: across the mount, so `mv` copies it,
# which reads it, and removes it from the folder.
file_moved_out_of_the_folder_takes_its_content_and_leaves_onedrive() {
    begin_folder
    seed_file A R downloaded.txt 'downloaded before it left'
    seed_file B R online.txt 'not downloaded before it left'
    seed_file C R stays.txt 'stays'
    register_folder
    download downloaded.txt
    $U sh -c "mkdir '$WORLD/out' && mv '$F/downloaded.txt' '$F/online.txt' '$WORLD/out/'"
    check "the downloaded file has its content where it went" "$($U cat "$WORLD/out/downloaded.txt")" 'downloaded before it left'
    check "the file that was not downloaded has its content where it went" "$($U cat "$WORLD/out/online.txt")" 'not downloaded before it left'
    check_within 30 "OneDrive lacks the downloaded file" onedrive_lacks downloaded.txt
    check_within 30 "OneDrive lacks the file that was not downloaded" onedrive_lacks online.txt
    check_sent
    check "they are in OneDrive's bin" "$(onedrive bin)" "A B"
    check "the folder" "$(listing)" "stays.txt"
}

# `mv` copies a file's extended attributes with it, and the mount lists its own among them.
file_moved_out_of_the_folder_carries_none_of_konedrives_attributes() {
    begin_folder
    seed_file A R a.txt 'downloaded before it left'
    register_folder
    download a.txt
    $U sh -c "mkdir '$WORLD/out' && mv '$F/a.txt' '$WORLD/out/'"
    check "the file has its content where it went" "$($U cat "$WORLD/out/a.txt")" 'downloaded before it left'
    check "and none of konedrive's attributes" "$($U python3 -c "import os,sys; print(sorted(a for a in os.listxattr(sys.argv[1]) if 'konedrive' in a))" "$WORLD/out/a.txt")" "[]"
}

# Create, edit, rename, move, a new directory and a removal, one after the other; then
# OneDrive holds what the folder holds, and a cycle that brings it all back changes nothing.
round_trip_leaves_onedrive_and_the_folder_alike() {
    begin_folder
    seed_folder D R Docs
    seed_file K D keep.txt 'KEEP'
    register_folder
    local id
    user "printf 'one' > rt.txt"
    check_onedrive_holds rt.txt 'one'
    check_sent
    id=$(onedrive id rt.txt)
    user "printf 'one two' > rt.txt"
    check_onedrive_holds rt.txt 'one two'
    check_sent
    user "mv rt.txt rt2.txt"
    check_within 30 "the rename is sent" onedrive_has rt2.txt
    check_sent
    user "mv rt2.txt Docs/"
    check_within 30 "the move is sent" onedrive_has Docs/rt2.txt
    check_sent
    check "one item was made, edited, renamed and moved" "$(onedrive id Docs/rt2.txt)" "$id"
    check_onedrive_holds Docs/rt2.txt 'one two'
    user "mkdir Docs/sub && printf 'deep' > Docs/sub/deep.txt"
    check_onedrive_holds Docs/sub/deep.txt 'deep'
    user "rm Docs/rt2.txt"
    check_within 30 "the removal is sent" onedrive_lacks Docs/rt2.txt
    check_sent
    check "the item is in OneDrive's bin" "$(onedrive bin)" "$id"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
    # The echo: a cycle brings everything back as it is.
    local changes deltas snapshot
    changes=$(onedrive changes)
    deltas=$(onedrive count GET delta)
    snapshot=$(user "find . -mindepth 1 -printf '%p %s %T@\n' | sort")
    daemon refresh > /dev/null
    a_cycle_ran() { [ "$(onedrive count GET delta)" -gt "$deltas" ]; }
    check_within 20 "a cycle ran" a_cycle_ran
    sleep 3
    check "the cycle sent nothing that changes OneDrive" "$(onedrive changes)" "$changes"
    check "nothing waits to be uploaded" "$(daemon upload-queue)" ""
    check "the cycle changed nothing in the folder" "$(user "find . -mindepth 1 -printf '%p %s %T@\n' | sort")" "$snapshot"
}

# The group: every scenario of writing at the level of files.
file_writes() {
    echo "==== writing at the level of files ===="
    ip link set lo up
    for name in \
        new_file_arrives_in_onedrive \
        new_directory_with_files_arrives_in_onedrive \
        edit_of_downloaded_file_is_uploaded \
        edit_of_file_that_is_not_downloaded_downloads_it_first \
        overwrite_in_place_of_file_that_is_not_downloaded_keeps_the_rest \
        removed_files_go_to_onedrives_bin \
        removed_directory_goes_from_onedrive \
        removing_a_large_directory_sends_every_removal \
        renamed_file_keeps_its_item \
        moved_file_keeps_its_item \
        renamed_and_moved_directory_keeps_its_items \
        save_by_rename_over_downloaded_file_is_one_update \
        save_by_rename_over_file_that_is_not_downloaded_is_one_update \
        save_by_rename_from_a_name_that_is_not_ignored_is_one_update \
        truncated_downloaded_file_is_uploaded_shorter \
        truncated_file_that_is_not_downloaded_keeps_its_start \
        file_truncated_to_nothing_is_uploaded_empty \
        file_opened_with_o_trunc_and_written_is_uploaded \
        file_opened_with_o_trunc_and_closed_is_uploaded_empty \
        file_changed_in_onedrive_before_the_upload_keeps_both \
        file_changed_in_onedrive_while_a_change_waits_keeps_both \
        file_removed_in_onedrive_while_changed_here_is_uploaded_again \
        file_changed_in_onedrive_while_removed_here_comes_back \
        read_only_folder_refuses_every_change \
        unsent_changes_wait_while_the_folder_is_read_only \
        forced_switch_to_read_only_keeps_unsent_changes_and_sends_them_once_read_write \
        file_held_open_for_writing_across_a_forced_switch_to_read_only_keeps_what_was_written \
        pinned_file_stays_downloaded \
        pinned_directory_keeps_what_is_in_it_downloaded \
        free_up_keeps_unsent_changes \
        daemon_killed_with_unsent_changes_sends_them_once_started_again \
        file_that_is_not_downloaded_moved_into_new_directory_is_read_there \
        tree_moved_in_from_outside_is_uploaded \
        file_moved_out_of_the_folder_takes_its_content_and_leaves_onedrive \
        file_moved_out_of_the_folder_carries_none_of_konedrives_attributes \
        round_trip_leaves_onedrive_and_the_folder_alike; do
        folder_scenario $name
    done
}
