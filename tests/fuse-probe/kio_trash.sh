# What KIO does when a file of a served folder is moved to the trash, as Dolphin asks for it
# (tests/kio/trash_probe.cpp). The folder has no trash of its own: the mount refuses to make
# `.Trash-<uid>` in it, and what is removed there goes to OneDrive's recycle bin. What is said
# here is what KDE Frameworks 6.31 does. Sourced by
# guest.sh, which gives $TRASH_PROBE; each scenario has a folder of its own
# (scenario_folder.sh), and a home of its own beside it, with its own trash: the home of
# whoever runs the suite is never touched.

# kio <plain|dolphin> <path in the folder>…: the probe run as the user, headless, on a session
# bus of its own, in the scenario's home; its RESULT lines.
kio() {
    # The runtime directory has a short path: KIO makes the socket of its worker in it, and a socket's path is short.
    local how=$1 home=$WORLD/home runtime=$W/kio-runtime paths=()
    shift
    for path in "$@"; do paths+=("$F/$path"); done
    rm -rf "$runtime"
    mkdir -p "$home/data" "$home/config" "$home/cache" "$runtime"
    chmod 700 "$runtime"
    chown -R 1000:1000 "$home" "$runtime"
    $U env HOME="$home" XDG_DATA_HOME="$home/data" XDG_CONFIG_HOME="$home/config" XDG_CACHE_HOME="$home/cache" \
        XDG_RUNTIME_DIR="$runtime" QT_QPA_PLATFORM=offscreen \
        timeout 90 dbus-run-session -- "$TRASH_PROBE" "$how" "${paths[@]}" 2> "$LOGS/kio-$how.err" | sed -n 's/^RESULT //p' | tr '\n' ';'
}

# What the home's trash holds, by name; and every trash directory made anywhere the scenario
# can reach: in its own directories, at the top of the disk, and in the folder as it is served.
home_trash() { (cd "$WORLD/home/data/Trash/files" 2> /dev/null && ls -A | tr '\n' ' '); }
trash_directories() {
    {
        find "$WORLD" "$W" -maxdepth 4 -name '.Trash*' -not -path "$F/*" 2> /dev/null
        user "ls -A | grep '^\.Trash'" | sed "s|^|$F/|"
    } | sort -u | tr '\n' ' '
}

# kio_trash_scene: a folder with a file that is on disk and one that is not.
kio_trash_scene() {
    seed_file A R on-disk.txt 'on disk'
    seed_random X R not-on-disk.bin 2097152
    seed_file K R kept.txt 'kept'
    register_folder
    download on-disk.txt
}

# check_no_trash_was_made <downloads asked before>: nothing went to the home's trash, no trash
# directory was made anywhere, and nothing was downloaded.
check_no_trash_was_made() {
    check "nothing went to the home's trash" "$(home_trash)" ""
    check "no trash directory was made anywhere" "$(trash_directories)" ""
    check "nothing was downloaded" "$(onedrive count GET dl/)" "$1"
}

# `KIO::trash` alone, as a program calls it that asks nothing: KIO (6.21 and later) finds no
# trash for the folder and says so; the file stays. An earlier KIO moved the file to the home's
# trash, by a copy across the mount, which this scenario would show as a download.
kio_trash_alone_says_no_trash_is_available_and_leaves_the_file() {
    begin_folder
    kio_trash_scene
    local asked
    asked=$(onedrive count GET dl/)
    check_holds "of a file on disk, KIO says that no trash is available" "$(kio plain on-disk.txt)" "error 180"
    check_holds "of a file that is not on disk, the same" "$(kio plain not-on-disk.bin)" "error 180"
    sleep 2
    check "the folder is as it was" "$(listing)" "kept.txt not-on-disk.bin on-disk.txt"
    check_no_trash_was_made "$asked"
    check "nothing was removed from OneDrive" "$(onedrive count DELETE items)" 0
}

# Dolphin's "Move to Trash" (`KIO::DeleteOrTrashJob`): when KIO says that no trash is available
# the user is asked whether to delete instead, and a yes deletes the file plainly: nothing is
# downloaded, and the removal goes to OneDrive, whose recycle bin keeps the file.
dolphins_move_to_trash_asks_to_delete_instead_and_downloads_nothing() {
    begin_folder
    kio_trash_scene
    local asked said
    asked=$(onedrive count GET dl/)
    said=$(kio dolphin on-disk.txt)
    check_holds "of a file on disk, the user is asked to delete it instead" "$said" "DeleteNoTrashAvailable"
    check_holds "and it is deleted" "$said" "job: no error"
    said=$(kio dolphin not-on-disk.bin)
    check_holds "of a file that is not on disk, the user is asked the same" "$said" "DeleteNoTrashAvailable"
    check_holds "and it is deleted" "$said" "job: no error"
    check "the folder lacks both" "$(listing)" "kept.txt"
    check_within 30 "the removal of the file that was on disk is sent" onedrive_lacks on-disk.txt
    check_within 30 "the removal of the file that was not on disk is sent" onedrive_lacks not-on-disk.bin
    check_sent
    check_no_trash_was_made "$asked"
    check "OneDrive's recycle bin holds both" "$(onedrive bin)" "A X"
    check "OneDrive holds what the folder holds" "$(onedrive paths)" "$(listing)"
}

# The group: every scenario of a file moved to the trash through KIO.
kio_trash() {
    echo "==== a file of the folder moved to the trash through KIO ===="
    ip link set lo up
    [ -x "${TRASH_PROBE:-}" ] || { echo "  FAILED  the trash probe was not built"; FAILED_SCENARIOS+=("kio_trash"); return; }
    for name in \
        kio_trash_alone_says_no_trash_is_available_and_leaves_the_file \
        dolphins_move_to_trash_asks_to_delete_instead_and_downloads_nothing; do
        folder_scenario $name
    done
}
