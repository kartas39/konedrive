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
