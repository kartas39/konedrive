use konedrive_dbus::overall::{Overall, Reason};
use super::{decide_read_only_asked, mode_shown_text, read_only_asked_text, status_text, AccountStatus, ReadOnlyAsked};

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
        reason: "start".into(),
        started: now - 130,
        ..Default::default()
    };
    assert_eq!(super::local_scan_text(&running, now), "running (2 min, as syncing started)");
    let idle = super::LocalScan { state: "idle".into(), finished: now - 300, took: 40, ..running.clone() };
    assert_eq!(super::local_scan_text(&idle, now), "last finished 5 min ago (took 40 s)");
    let never = super::LocalScan { state: "idle".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&never, now), "not yet since the daemon started");
    let none = super::LocalScan { state: "none".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&none, now), "not yet since the daemon started");
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
        overall: Some(Reason::Conflicts.into()),
        trouble: "one file could not be placed".into(),
        not_updated: String::new(),
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
         Overall:                warning — changed files were moved out of the way\n\
         State:                  ready\n\
         Files:                  a file that is not downloaded is read from OneDrive when something reads it\n\
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
         Paused until:           resumed (`{p} sync resume`)\n\
         Paused by itself:       metered connection (`{p} sync anyway` syncs now)\n\
         On this computer:       5.0 MiB\n\
         Always on this device:  7\n\
         Conflicts:              1 (see `{p} sync conflicts`)\n",
        waiting = crate::text::uploads::waiting_text(2, 2048),
        space = crate::text::uploads::space_waiting_text(4, 1 << 30),
    );
    assert_eq!(super::sync_status_text(&status, p, now), expected);

    let none = super::FolderStatus { state: Some("none".into()), ..Default::default() };
    assert_eq!(
        super::sync_status_text(&none, p, now),
        "Folder:                 (none)\nState:                  none\n",
        "an account with no folder, in a list of several"
    );
}

/// A daemon of an older build has not every property this build reads: a value it does not
/// have is absent, its line is left out, and the rest is printed. `Overall` is one of them:
/// without it no state of the whole is printed, and none is worked out here.
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
        super::sync_status_text(&folder, "konedrivectl", 1_000),
        "Folder:                 /home/u/OneDrive\n\
         State:                  ready\n\
         Files:                  a file that is not downloaded is read from OneDrive when something reads it\n\
         Items:                  3 in OneDrive, 3 in the folder\n"
    );
    let account = super::AccountStatus { state: Some("signed-in".into()), email: "ann@outlook.com".into(), ..Default::default() };
    assert_eq!(super::status_text(&account, Some("id")), "State:      signed-in\nClient ID:  id\n");
}

/// The `Overall:` line says the state the daemon decided, in words by its reason; the
/// sentence of `Trouble` or of `NotUpdated` is printed for the reasons that are about one,
/// and for no other.
#[test]
fn the_overall_line_says_the_state_in_words_by_the_reason() {
    let text = |reason: Reason, trouble: &str| super::overall_text(&reason.into(), trouble, "");
    assert_eq!(text(Reason::UpToDate, ""), "ok — up to date");
    assert_eq!(text(Reason::SignedOut, ""), "offline — signed out of OneDrive");
    assert_eq!(text(Reason::Listing, ""), "syncing — listing your OneDrive");
    assert_eq!(text(Reason::Paused, ""), "paused — you paused syncing");
    assert_eq!(text(Reason::HeldBack, ""), "paused — the account holds back by itself");
    assert_eq!(text(Reason::Blocked, ""), "warning — changes cannot be uploaded");

    let unreachable = "cannot reach OneDrive (timed out); trying again";
    assert_eq!(text(Reason::Unreachable, unreachable), format!("offline — {unreachable}"));
    assert_eq!(text(Reason::Trouble, "part of the folder is scanned"), "warning — part of the folder is scanned");
    let note = "1 file(s) changed in OneDrive could not be updated here yet: no space";
    assert_eq!(super::overall_text(&Reason::NotUpdated.into(), "part of the folder is scanned", note), format!("warning — {note}"));
    // Under a reason that ranks higher the two sentences are there too, and are not said
    // on this line.
    assert_eq!(super::overall_text(&Reason::Paused.into(), "part of the folder is scanned", note), "paused — you paused syncing");
    assert_eq!(text(Reason::Stopped, "signed out"), "warning — syncing has stopped: signed out");
    assert_eq!(text(Reason::Stopped, ""), "warning — syncing has stopped");

    // Every reason has words of its own, and its state in front of them.
    let mut said = std::collections::BTreeSet::new();
    for reason in Reason::ALL {
        let line = text(reason, "");
        assert!(line.starts_with(&format!("{} — ", reason.state().as_str())), "{line}");
        assert!(said.insert(line.clone()), "{line} is said for two reasons");
    }
    // A reason of a newer daemon is printed as it is spelled, under the state it came with.
    let newer = Overall { state: "warning".into(), reason: "on-fire".into() };
    assert_eq!(super::overall_text(&newer, "", ""), "warning — on-fire");
    assert_eq!(super::overall_text(&newer, "part of the folder is scanned", ""), "warning — on-fire");
}

/// `status` prints the same line under the sign-in's state.
#[test]
fn status_prints_the_overall_line() {
    let account = super::AccountStatus {
        state: Some("signed-in".into()),
        overall: Some(Reason::Unreachable.into()),
        trouble: "cannot reach OneDrive (timed out); trying again".into(),
        ..Default::default()
    };
    assert_eq!(
        super::status_text(&account, None),
        "State:      signed-in
Overall:    offline — cannot reach OneDrive (timed out); trying again
"
    );
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

/// An account that waits to switch to read-only: `status` says it with the mode, beside
/// whatever `LastError` says; `account mode` adds the two ways out.
#[test]
fn a_switch_to_read_only_that_waits_is_shown_with_its_count() {
    let waits = AccountStatus { mode: Some("read-write".to_owned()), read_only_wish: Some(1), ..AccountStatus::default() };
    assert_eq!(status_text(&waits, None), "Mode:       read-write, switching to read-only: 1 change still to send\n");
    let failed = AccountStatus { last_error: "no network".to_owned(), ..waits.clone() };
    assert_eq!(
        status_text(&failed, None),
        "Mode:       read-write, switching to read-only: 1 change still to send\nLast error: no network\n"
    );
    let plain = AccountStatus { read_only_wish: None, ..failed.clone() };
    assert_eq!(status_text(&plain, None), "Mode:       read-write\nLast error: no network\n");

    let shown = mode_shown_text("Test: ", "konedrivectl --account Test", "read-write", Some(3), "");
    assert!(shown.starts_with("Test: read-write\nSwitching to read-only: 3 changes still to send. "), "{shown}");
    assert!(shown.contains("`konedrivectl --account Test account mode read-only --force` switches now"), "{shown}");
    assert!(shown.contains("`konedrivectl --account Test account mode read-write` cancels"), "{shown}");
    assert!(!shown.contains("Last error"), "{shown}");
    assert_eq!(mode_shown_text("", "konedrivectl", "read-only", None, ""), "read-only\n");
    assert_eq!(mode_shown_text("", "konedrivectl", "read-only", None, "no client id"), "read-only\nLast error: no client id\n");
}

/// `account mode read-only` says what came of it: switched, or waiting with the count and
/// the two ways out; a case per answer of the account.
#[test]
fn read_only_asked_says_whether_it_switched_or_waits() {
    assert_eq!(decide_read_only_asked("read-only", None), ReadOnlyAsked::Switched);
    assert_eq!(decide_read_only_asked("read-only", Some(2)), ReadOnlyAsked::Switched);
    assert_eq!(decide_read_only_asked("read-write", Some(2)), ReadOnlyAsked::Waits { unsent: 2 });
    assert_eq!(decide_read_only_asked("read-write", Some(0)), ReadOnlyAsked::Waits { unsent: 0 });
    assert_eq!(decide_read_only_asked("read-write", None), ReadOnlyAsked::NotSaid);

    let switched = read_only_asked_text("", "konedrivectl", ReadOnlyAsked::Switched);
    assert!(switched.starts_with("Read-only: ") && switched.contains("stays on this computer"), "{switched}");
    let waits = read_only_asked_text("Test: ", "konedrivectl --account Test", ReadOnlyAsked::Waits { unsent: 2 });
    assert!(waits.starts_with("Test: Switching to read-only: 2 changes still to send. It switches by itself"), "{waits}");
    assert!(waits.contains("account mode read-only --force") && waits.contains("account mode read-write` cancels"), "{waits}");
    let not_said = read_only_asked_text("", "konedrivectl", ReadOnlyAsked::NotSaid);
    assert!(not_said.contains("`konedrivectl account mode`"), "{not_said}");
}
