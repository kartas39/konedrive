/// Issue #54: `sync status` says whether changes from OneDrive arrive live, and nothing
/// while the socket is off (the pause or the hold says why).
#[test]
fn the_live_changes_line_says_live_or_every_minute() {
    assert_eq!(super::live_text("connected"), Some("live"));
    assert_eq!(super::live_text("connecting"), Some("every minute (connecting)"));
    assert_eq!(super::live_text("off"), None);
}

#[test]
fn the_local_scan_line_says_how_far_it_got_or_when_it_last_finished() {
    let now = 1_000_000;
    let running = super::LocalScan {
        state: "running".into(),
        reason: "read-write".into(),
        started: now - 130,
        directories: 1_234,
        files: 45_678,
        expected: 50_000,
        ..Default::default()
    };
    assert_eq!(
        super::local_scan_text(&running, now),
        "running — 1 234 folders and 45 678 files, of about 50 000 (2 min, after the switch to read-write)"
    );
    let idle = super::LocalScan { state: "idle".into(), finished: now - 300, took: 40, ..running.clone() };
    assert_eq!(super::local_scan_text(&idle, now), "last finished 5 min ago (took 40 s)");
    let never = super::LocalScan { state: "idle".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&never, now), "not yet since the daemon started");
    let none = super::LocalScan { state: "none".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&none, now), "none — read-only");
}

/// `sync status` of a folder that shows OneDrive: every line there can be, in its order, with
/// the commands it suggests starting as `prefix` says; and of an account with no folder.
#[test]
fn sync_status_prints_every_line_of_a_folder_that_shows_onedrive() {
    let now = 1_000_000;
    let status = super::FolderStatus {
        path: "/home/u/OneDrive".into(),
        state: "ready".into(),
        last_error: "one file could not be placed".into(),
        source: "onedrive".into(),
        items_listed: 120,
        items_placed: 118,
        skipped: 2,
        last_checked: now - 20,
        live_changes: "connected".into(),
        mode: "read-write".into(),
        download_left: 3,
        download_left_bytes: 3 << 20,
        scan: super::LocalScan { state: "idle".into(), finished: now - 300, took: 40, ..Default::default() },
        pending: 2,
        pending_bytes: 2048,
        blocked: 1,
        quota_full: true,
        quota_waiting: 4,
        quota_waiting_bytes: 1 << 30,
        too_big: 1,
        held_deletes: 60,
        paused: true,
        paused_until: 0,
        held_back: "metered".into(),
        local_bytes: 5 << 20,
        pinned: 7,
        conflicts: 1,
    };
    let p = "konedrivectl --account Work";
    let expected = format!(
        "Folder:                 /home/u/OneDrive\n\
         State:                  ready\n\
         Opens:                  intercepted: a file is downloaded when something opens it\n\
         Helper:                 connected\n\
         Last error:             one file could not be placed\n\
         Items:                  120 in OneDrive, 118 in the folder\n\
         Skipped:                2 (see `{p} sync skipped`)\n\
         Last checked:           20 s ago\n\
         Changes from OneDrive:  live\n\
         Mode:                   read-write: changes made here are uploaded\n\
         Waiting to download:    3 files (3.0 MiB)\n\
         Local scan:             last finished 5 min ago (took 40 s)\n\
         Waiting to upload:      {waiting}\n\
         Blocked:                1 (see `{p} sync not-uploaded`)\n\
         Waiting for space:      {space}\n\
         Too big for the space:  1 (see `{p} sync outbox`)\n\
         Held for confirmation:  60 deletions (`{p} sync deletes confirm` or `{p} sync deletes restore`)\n\
         Paused until:           resumed (`{p} sync resume`)\n\
         Paused by itself:       metered connection (`{p} sync anyway` syncs now)\n\
         On this computer:       5.0 MiB\n\
         Always on this device:  7\n\
         Conflicts:              1 (see `{p} sync conflicts`)\n",
        waiting = crate::text::uploads::waiting_text(2, 2048),
        space = crate::text::uploads::space_waiting_text(4, 1 << 30),
    );
    assert_eq!(super::sync_status_text(&status, Some("connected"), p, now), expected);

    let none = super::FolderStatus { state: "none".into(), ..Default::default() };
    assert_eq!(
        super::sync_status_text(&none, None, p, now),
        "Folder:                 (none)\nState:                  none\n",
        "an account with no folder, in a list of several"
    );
    assert_eq!(
        super::helper_text("stopped"),
        "stopped — the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`"
    );
}
