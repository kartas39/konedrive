use super::*;

/// A snapshot whose folder is `folder`, the rest as at the start.
fn folder(folder: FolderStatus) -> SyncSnapshot {
    SyncSnapshot { folder, ..SyncSnapshot::default() }
}

/// The throttle's note names the minute the wait is over in: a time within a minute is
/// rounded up, so the note never names a minute that is past while the wait lasts.
#[test]
fn the_throttles_time_is_rounded_up_to_the_minute() {
    let minute = 1_700_000_040; // a multiple of 60
    assert_eq!(minute % 60, 0);
    let said = |at| UploadQueueNote::Throttled(at).text();
    assert_eq!(said(minute + 1), said(minute + 60), "a second into the minute reads as the next minute");
    assert_eq!(said(minute + 59), said(minute + 60));
    assert_ne!(said(minute), said(minute + 1), "the full minute reads as itself");
}

/// The worker's notes are said only where nothing else is, the folder's over the
/// throttle's, and the worker takes back only its own; a throttle shorter than five
/// seconds is not said, and one that is said stays until its end.
#[test]
fn the_worker_takes_back_only_its_own_notes() {
    const NOW: i64 = 1_000;
    let throttled = |until| Some(UploadQueueNote::Throttled(until));
    let closed = Some(UploadQueueNote::FolderClosed("EACCES".into()));
    let gate = Some(UploadQueueNote::GateClosed("why".into()));
    let after = |shown: &Option<UploadQueueNote>, folder: Option<&str>, until: Option<i64>, now: i64| UploadQueueNote::after_worker(shown, folder, until, now);
    assert_eq!(after(&None, None, Some(NOW + 100), NOW), Some(throttled(NOW + 100)));
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 100), NOW + 99), None, "said, it stays to its end");
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 160), NOW), Some(throttled(NOW + 160)));
    assert_eq!(after(&throttled(NOW + 100), None, None, NOW + 100), Some(None), "its own note goes as the wait ends");
    assert_eq!(after(&None, None, Some(NOW + 4), NOW), None, "a wait under five seconds is not said");
    assert_eq!(after(&None, None, Some(NOW + 5), NOW), Some(throttled(NOW + 5)));
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 3), NOW), Some(None), "a short wait that took a long one's place is not said either");
    assert_eq!(after(&gate, None, Some(NOW + 100), NOW), None, "the gate's stays");
    assert_eq!(after(&gate, Some("EACCES"), None, NOW), None);
    assert_eq!(after(&gate, None, None, NOW), None);
    assert_eq!(after(&Some(UploadQueueNote::HeldBack(3)), None, None, NOW), None, "the poller's stays");
    assert_eq!(after(&None, None, None, NOW), None);
    // The folder that cannot be opened is said over the throttle, and goes when it opens.
    assert_eq!(after(&throttled(NOW + 100), Some("EACCES"), Some(NOW + 100), NOW), Some(closed.clone()));
    assert_eq!(after(&closed, Some("EACCES"), None, NOW), None, "nothing changes");
    assert_eq!(after(&closed, None, Some(NOW + 100), NOW), Some(throttled(NOW + 100)), "opened, the throttle that still lasts is said");
    assert_eq!(after(&closed, None, None, NOW), Some(None));
    // The gate closing says so over the worker's notes, and takes neither back.
    assert_eq!(UploadQueueNote::after_gate(&throttled(100), Some("why")), Some(gate.clone()));
    assert_eq!(UploadQueueNote::after_gate(&throttled(100), None), None, "the gate does not take the throttle's note");
    assert_eq!(UploadQueueNote::after_gate(&closed, None), None);
}

/// The write gate says why it is closed over whatever is shown, and takes back only its
/// own note: what the poller says of a read-only folder stays.
#[test]
fn the_gate_takes_back_only_its_own_note() {
    let closed = |why: &str| Some(UploadQueueNote::GateClosed(why.to_owned()));
    assert_eq!(UploadQueueNote::after_gate(&None, Some("why")), Some(closed("why")));
    assert_eq!(UploadQueueNote::after_gate(&closed("why"), Some("why")), None, "nothing changes");
    assert_eq!(UploadQueueNote::after_gate(&closed("why"), None), Some(None), "its own note goes as it opens");
    assert_eq!(UploadQueueNote::after_gate(&Some(UploadQueueNote::HeldBack(3)), None), None, "the poller's stays");
    assert_eq!(UploadQueueNote::after_gate(&Some(UploadQueueNote::HeldBack(3)), Some("why")), Some(closed("why")));
    assert_eq!(UploadQueueNote::after_gate(&None, None), None);
}

#[test]
fn the_published_state_is_computed_from_the_registration_and_the_sync() {
    let mut s = folder(FolderStatus { root_state: RootState::Ready, ..FolderStatus::default() });
    assert_eq!(published_state(&s), "ready");
    s.cycle.listing = true;
    assert_eq!(published_state(&s), "listing");
    s.cycle.sync_trouble = Some(SyncTrouble { text: "cannot reach OneDrive".into(), blocking: false, kind: TroubleKind::Unreachable });
    assert_eq!(published_state(&s), "listing", "no network is said, not an error");
    s.cycle.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true, kind: TroubleKind::Other });
    assert_eq!(published_state(&s), "error");
    s.folder.root_state = RootState::Error;
    s.folder.last_error = "the folder cannot be mounted".into();
    s.local.conflict_count = 1;
    assert_eq!(published_error(&s), "the folder cannot be mounted. signed out", "a conflict is not a problem; it is not in LastError");
    assert_eq!(published_state(&SyncSnapshot::default()), "none");
}
