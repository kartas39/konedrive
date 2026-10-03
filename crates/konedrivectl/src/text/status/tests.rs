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
    assert_eq!(super::grouped(999), "999");
    assert_eq!(super::grouped(1_000_000), "1 000 000");
    assert_eq!(super::seconds_text(3_900), "1 h 5 min");
}
