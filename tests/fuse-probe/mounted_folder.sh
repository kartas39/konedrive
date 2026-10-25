# The scenarios of a folder from its start to its end: what OneDrive holds shown and read, what
# OneDrive changes arriving, a directory that cannot be removed here, a name that cannot be used
# here, and the folder forgotten.
# Sourced by guest.sh; each scenario has a folder of its own (scenario_folder.sh).

start_from_nothing_shows_onedrives_files_and_reads_them() {
    begin_folder
    seed_folder D R docs
    seed_folder E R empty
    seed_file A R a.txt 'at the top'
    seed_file B D b.txt 'inside docs'
    register_folder read-only
    check "the folder is a mount of the daemon's" "$(awk -v m="$F" '$2 == m { print $3 }' /proc/self/mounts)" fuse.konedrive
    check_holds "the folder is ready" "$(daemon status)" "state Ready"
    check "the folder shows what OneDrive holds" "$(listing)" "a.txt docs docs/b.txt empty"
    check "a file that was never read has its size and takes no space" "$(user "stat -c '%s %b' docs/b.txt")" "11 0"
    check "and is not downloaded" "$(state_of docs/b.txt)" online-only
    check "nothing was downloaded to show the folder" "$(onedrive count GET dl/)" 0
    check_folder_holds docs/b.txt 'inside docs'
    check_folder_holds a.txt 'at the top'
    check_state docs/b.txt hydrated
    check "a read-only folder takes no change" "$(errno_of "os.mkdir('made')")" EROFS
    check "nothing was changed in OneDrive" "$(onedrive changes)" 0
}

# Parts of a file that is not on disk are read from OneDrive as they are asked for; read from
# its start to its end, the file is downloaded, and its copy stays in the folder itself.
file_that_is_not_on_disk_is_read_in_ranges_and_then_downloaded_whole() {
    begin_folder
    seed_random X R big.bin 20971520
    register_folder read-only
    check "the start of the file is OneDrive's" "$(cmp <(user "head -c 32768 big.bin") <(head -c 32768 "$SRC/seed-X") && echo equal)" equal
    check "bytes of its middle are OneDrive's" "$(cmp <(user "dd if=big.bin bs=4096 skip=2000 count=4 2> /dev/null") <(dd if="$SRC/seed-X" bs=4096 skip=2000 count=4 2> /dev/null) && echo equal)" equal
    check "a look at parts of it does not download it" "$(state_of big.bin) $(user "stat -c %b big.bin")" "online-only 0"
    check "read whole, it is OneDrive's" "$(is_seed big.bin X && echo equal)" equal
    check_state big.bin hydrated
    check "the copy takes the file's space" "$([ "$(user "stat -c %b big.bin")" -ge 40960 ] && echo yes || user "stat -c %b big.bin")" yes
    check "no download is left unfinished" "$(daemon staged)" "[]"
    local asked
    asked=$(onedrive count GET dl/X)
    check "a second read asks OneDrive for nothing" "$(is_seed big.bin X && echo equal) $(onedrive count GET dl/X)" "equal $asked"
    check "the service stops by itself" "$(stop_service)" Finished
    check "the folder itself holds the copy, the daemon stopped" "$($U cmp -s "$F/big.bin" "$SRC/seed-X" && echo equal)" equal
}

changes_made_in_onedrive_arrive_in_the_folder() {
    begin_folder
    seed_folder D R docs
    seed_file A R edited.txt 'the first version'
    seed_file B R renamed.txt 'renamed'
    seed_file C R removed.txt 'removed'
    seed_file E D moved.txt 'moved'
    seed_file G R untouched.txt 'untouched'
    register_folder read-only
    download edited.txt
    download removed.txt
    printf 'changed in OneDrive' > "$SRC/theirs"
    onedrive edit A "$SRC/theirs" > /dev/null
    onedrive rename B R renamed-there.txt > /dev/null
    onedrive trash C > /dev/null
    onedrive rename E R moved.txt > /dev/null
    seed_folder N R arrived
    seed_file H N new.txt 'arrived later'
    daemon refresh > /dev/null
    check_within 20 "the new directory and its file arrive" folder_has arrived/new.txt
    check_within 20 "the removed file goes" folder_lacks removed.txt
    wait_for 10 user "grep -q 'in OneDrive' edited.txt"
    check "the folder shows what OneDrive holds now" "$(listing)" "arrived arrived/new.txt docs edited.txt moved.txt renamed-there.txt untouched.txt"
    check_folder_holds edited.txt 'changed in OneDrive'
    check_folder_holds renamed-there.txt 'renamed'
    check_folder_holds moved.txt 'moved'
    check_folder_holds arrived/new.txt 'arrived later'
    check "a file nothing happened to is still not downloaded" "$(state_of untouched.txt)" online-only
    check "nothing was changed in OneDrive by it" "$(onedrive changes)" 0
    check "the service stops by itself" "$(stop_service)" Finished
    check "nothing of the removed file is left in the folder itself" "$($U ls -A "$F" | grep -c removed)" 0
}

# The owner's decision of 2026-10-11: a directory that holds in OneDrive something the folder
# cannot show cannot be removed here; what it shows is removed as asked.
directory_holding_what_the_folder_cannot_show_is_not_removed() {
    begin_folder
    local long
    long=$(printf 'x%.0s' $(seq 300)).txt
    seed_folder D R docs
    seed_file A D a.txt 'shown'
    seed_file L D "$long" 'its name is too long for the folder'
    register_folder
    check "the folder shows what it can" "$(listing)" "docs docs/a.txt"
    user "rm docs/a.txt"
    check "the directory is not removed" "$(errno_of "os.rmdir('docs')")" ENOTEMPTY
    check_within 30 "the removal of the shown file is sent" onedrive_lacks docs/a.txt
    check_sent
    check "the folder still shows the directory" "$(listing)" "docs"
    check "OneDrive still holds the directory and what could not be shown" "$(onedrive id docs) $(onedrive id "docs/$long")" "D L"
    check "one removal was sent" "$(onedrive count DELETE items)" 1
}

# The owner's decision of 2026-10-25: a name OneDrive holds with an item the folder cannot show
# is refused here at once, by mkdir and by rename, as OneDrive compares names, and the user is
# told once; nothing is renamed after the fact.
name_held_by_what_the_folder_cannot_show_is_refused() {
    begin_folder
    seed_folder D R Documents
    seed_notebook N D IT
    register_folder
    check "the folder shows the directory and not the notebook" "$(listing)" "Documents"
    user "mkdir Documents/IT-soname-krasnov"
    check_within 30 "the directory made here is sent" onedrive_has Documents/IT-soname-krasnov
    check "no directory is made at the notebook's name" "$(errno_of "os.mkdir('Documents/IT')")" EEXIST
    check "nor at a name that differs by case" "$(errno_of "os.mkdir('Documents/it')")" EEXIST
    check "a rename to it is refused" "$(errno_of "os.rename('Documents/IT-soname-krasnov', 'Documents/IT')")" EEXIST
    user "printf 'mine' > notes.txt"
    # What mv says of the refusal is coreutils' own: it finds nothing at the name, and is told it is taken.
    check "mv of a directory to it is refused" "$(user "mv Documents/IT-soname-krasnov Documents/IT 2>&1")" \
        "mv: cannot overwrite non-directory 'Documents/IT' with directory 'Documents/IT-soname-krasnov'"
    check "mv of a file to it is refused" "$(user "mv notes.txt Documents/IT 2>&1")" "mv: cannot overwrite 'Documents/IT': File exists"
    check "each stays where it was" "$(listing)" "Documents Documents/IT-soname-krasnov notes.txt"
    check "the user is told once" "$(daemon activity | grep -o NameTakenInOneDrive | wc -l)" 1
    check_sent
    check "OneDrive holds the notebook and the rest as they were" "$(onedrive paths)" "Documents Documents/IT Documents/IT-soname-krasnov notes.txt"
}

# The owner's decision of 2026-10-11: Forget is never refused for what is unsent; the unsent
# files stay only here, as plain files of the user's.
forgotten_folder_with_unsent_changes_keeps_them_as_plain_files() {
    begin_folder
    seed_file A R a.txt 'the first version'
    seed_file B R online.txt 'never read'
    register_folder
    download a.txt
    onedrive outage on > /dev/null
    user "printf 'changed here' > a.txt && printf 'made here' > new.txt"
    wait_for 10 upload_queue_has_rows
    check "forgotten, though changes are unsent" "$(daemon forget)" forgotten
    check "the mount is off the folder" "$(mounted "$F" && echo mounted || echo off)" off
    check "the folder itself holds the files that were on disk" "$($U ls -A "$F" | tr '\n' ' ')" "a.txt new.txt "
    check "with the content changed here" "$($U cat "$F/a.txt" "$F/new.txt")" 'changed heremade here'
    check "and is the user's again" "$($U stat -c %a "$F" "$F/a.txt" "$F/new.txt" | tr '\n' ' ')" "755 644 644 "
    onedrive outage off > /dev/null
    sleep 2
    check "nothing was sent" "$(onedrive changes)" 0
    check "OneDrive still holds its version" "$(onedrive compare a.txt "$SRC/seed-A")" equal
}

# The group: every scenario of a folder from its start to its end.
mounted_folder() {
    echo "==== a folder from its start to its end ===="
    ip link set lo up
    for name in \
        start_from_nothing_shows_onedrives_files_and_reads_them \
        file_that_is_not_on_disk_is_read_in_ranges_and_then_downloaded_whole \
        changes_made_in_onedrive_arrive_in_the_folder \
        directory_holding_what_the_folder_cannot_show_is_not_removed \
        name_held_by_what_the_folder_cannot_show_is_refused \
        forgotten_folder_with_unsent_changes_keeps_them_as_plain_files; do
        folder_scenario $name
    done
}
