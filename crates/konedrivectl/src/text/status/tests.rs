/// `sync status` says whether changes from OneDrive arrive live, and nothing
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
        state: Some("ready".into()),
        last_error: "one file could not be placed".into(),
        source: "onedrive".into(),
        items: Some((120, 118)),
        skipped: 2,
        last_checked: Some(now - 20),
        live_changes: "connected".into(),
        mode: Some("read-write".into()),
        writable: Some(true),
        download_left: Some((3, 3 << 20)),
        scan: Some(super::LocalScan { state: "idle".into(), finished: now - 300, took: 40, ..Default::default() }),
        pending: Some((2, 2048)),
        blocked: 1,
        quota_full: true,
        quota_waiting: 4,
        quota_waiting_bytes: 1 << 30,
        too_big: 1,
        held_deletes: 60,
        paused: true,
        paused_until: 0,
        held_back: "metered".into(),
        local_bytes: Some(5 << 20),
        pinned: Some(7),
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

    let none = super::FolderStatus { state: Some("none".into()), ..Default::default() };
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

/// A daemon of an older build has not every property this build reads: a value it does not
/// have is absent, its line is left out, and the rest is printed.
#[test]
fn a_value_the_daemon_does_not_have_leaves_its_line_out() {
    let folder = super::FolderStatus {
        path: "/home/u/OneDrive".into(),
        state: Some("ready".into()),
        source: "onedrive".into(),
        items: Some((3, 3)),
        ..Default::default()
    };
    assert_eq!(
        super::sync_status_text(&folder, None, "konedrivectl", 1_000),
        "Folder:                 /home/u/OneDrive\n\
         State:                  ready\n\
         Opens:                  intercepted: a file is downloaded when something opens it\n\
         Items:                  3 in OneDrive, 3 in the folder\n"
    );
    let account = super::AccountStatus { state: Some("signed-in".into()), email: "ann@outlook.com".into(), ..Default::default() };
    assert_eq!(super::status_text(&account, Some("id")), "State:      signed-in\nClient ID:  id\n");
}

/// A folder that is not writable though its account is read-write reads so in the `Mode:`
/// line; a read-only account reads as before, whatever the folder.
#[test]
fn the_mode_line_says_a_read_write_account_whose_folder_is_read_only_for_now() {
    assert!(super::mode_text("read-write", Some(false)).contains("read-only for now"));
    assert!(super::mode_text("read-write", Some(true)).contains("changes made here are uploaded"));
    assert_eq!(super::mode_text("read-only", Some(false)), super::mode_text("read-only", Some(true)));
    // A daemon that does not say leaves the account's mode alone.
    assert_eq!(super::mode_text("read-write", None), super::mode_text("read-write", Some(true)));
}
