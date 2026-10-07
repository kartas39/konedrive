use crate::outbox::{Detection, OutboxKind, OutboxOp, OutboxState, SessionUrl, Snapshot};
use crate::TreeStore;

/// A row's snapshot comes back as it was written, whatever the time (one
/// before 1970, one past what nanoseconds in 64 bits hold), and so do a
/// `move-out` row's markers; a session opened for other content goes with
/// the content. A marker no konedrive writes blocks the row. The forms of
/// the target name are told apart.
#[test]
fn a_rows_snapshot_and_target_name_are_read_as_written() {
    let mut s = TreeStore::in_memory().unwrap();
    let d = Detection {
        kind: OutboxKind::Create,
        item_id: None,
        inode: None,
        rel: "a.txt".into(),
        base: None,
        target_parent: None,
        target_name: Some("a.txt".into()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    };
    s.outbox_apply(&[OutboxOp::Record(d)], 1).unwrap();
    let seq = s.outbox_rows().unwrap()[0].seq;

    let content = Snapshot::content(100, 2, 5);
    assert_eq!(s.outbox_take_snapshot(seq, content).unwrap(), None);
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.snapshot_size(), row.snapshot_sent()), (Some(content), Some(100), Some((100, 2))));
    assert!(row.snapshot_is(content) && !row.snapshot_is(Snapshot::content(100, 2, 6)));
    s.outbox_open_session(seq, &SessionUrl::new("https://up.example/s"), Some(50), None, 10).unwrap();
    assert_eq!(s.outbox_take_snapshot(seq, content).unwrap(), None, "the same content keeps its session");
    let sending = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!(sending.session_url.as_ref().map(SessionUrl::as_str), Some("https://up.example/s"));
    assert!(!format!("{sending:?}").contains("up.example"), "a session's URL is a credential: a row printed for the journal does not show it");
    for other in [Snapshot::content(1, -1, 5), Snapshot::content(1, i64::MAX, 999_999_999), Snapshot::content(1, i64::MIN, 0)] {
        let dropped = s.outbox_take_snapshot(seq, other).unwrap();
        let row = s.outbox_row(seq).unwrap().unwrap();
        assert_eq!((row.snapshot(), row.session_url.as_ref().map(SessionUrl::as_str)), (Some(other), None));
        assert_eq!(dropped.is_some(), other == Snapshot::content(1, -1, 5), "the session of the content before goes, once");
    }

    for marker in [Snapshot::ContentLocal, Snapshot::Trashed] {
        s.outbox_set_snapshot(seq, Some(marker)).unwrap();
        let row = s.outbox_row(seq).unwrap().unwrap();
        assert_eq!((row.snapshot(), row.snapshot_size(), row.snapshot_sent()), (Some(marker), None, None));
        assert!(marker.is_marker() && row.snapshot_is(marker));
    }
    s.bench_sql("UPDATE outbox SET moved_out = 'elsewhere'").unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.state), (None, OutboxState::Blocked));
    assert!(row.reason_text().unwrap().contains("elsewhere"));
    s.outbox_set_snapshot(seq, None).unwrap();
    assert_eq!((s.outbox_row(seq).unwrap().unwrap().snapshot(), s.outbox_row(seq).unwrap().unwrap().state), (None, OutboxState::Ready));

    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (None, None), "a name");
    s.outbox_set_target(seq, Some("R"), Some(".konedrive-swap-7")).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (Some(".konedrive-swap-7"), None));
    s.outbox_set_target(seq, None, crate::outbox::place_name(std::path::Path::new("/home/u/a.txt"))).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (None, Some(std::path::Path::new("/home/u/a.txt"))));
}
