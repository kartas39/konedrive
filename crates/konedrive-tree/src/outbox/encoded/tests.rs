use crate::outbox::{Detection, OutboxKind, OutboxOp, OutboxState, SessionUrl, Snapshot};
use crate::TreeStore;

/// A row's snapshot comes back as it was written, whatever the time (one
/// before 1970, one past what nanoseconds in 64 bits hold); a session
/// opened for other content goes with the content. A temporary name in the
/// target name is told from a name.
#[test]
fn a_rows_snapshot_and_target_name_are_read_as_written() {
    let mut s = TreeStore::in_memory().unwrap();
    let d = Detection {
        local_item_id: None,
        kind: OutboxKind::Create,
        item_id: None,
        object: None,
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

    s.outbox_set_snapshot(seq, None).unwrap();
    assert_eq!((s.outbox_row(seq).unwrap().unwrap().snapshot(), s.outbox_row(seq).unwrap().unwrap().state), (None, OutboxState::Ready));

    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!(row.swap_name(), None, "a name");
    s.outbox_set_target(seq, Some("R"), Some(".konedrive-swap-7")).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!(row.swap_name(), Some(".konedrive-swap-7"));
}

/// The content being sent is told by its change count too, where the row's item has one:
/// content of one size and time is another content once a program has opened the file for
/// writing, and a session opened for the content before goes. The count is read as written,
/// goes with the snapshot, and a snapshot taken with none has none.
#[test]
fn a_rows_snapshot_holds_the_change_count_of_the_content_being_sent() {
    let mut s = TreeStore::in_memory().unwrap();
    let d = Detection {
        local_item_id: None,
        kind: OutboxKind::Update,
        item_id: Some("A".into()),
        object: None,
        rel: "a.txt".into(),
        base: None,
        target_parent: None,
        target_name: None,
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    };
    s.outbox_apply(&[OutboxOp::Record(d)], 1).unwrap();
    let seq = s.outbox_rows().unwrap()[0].seq;

    let sent = Snapshot::content(9, 100, 5).with_change_count(1);
    s.outbox_take_snapshot(seq, sent).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.snapshot_change_count(), row.snapshot_sent()), (Some(sent), Some(1), Some((9, 100))));
    assert!(row.snapshot_is(sent));
    assert!(!row.snapshot_is(Snapshot::content(9, 100, 5).with_change_count(2)), "a writer opened the file since: not the content that was sent");
    assert!(!row.snapshot_is(Snapshot::content(9, 100, 5)), "a snapshot with no count is not one with a count");

    s.outbox_open_session(seq, &SessionUrl::new("https://up.example/s"), Some(50), None, 10).unwrap();
    let dropped = s.outbox_take_snapshot(seq, Snapshot::content(9, 100, 5).with_change_count(2)).unwrap();
    assert!(dropped.is_some(), "the session of the content before the open goes");
    assert_eq!(s.outbox_row(seq).unwrap().unwrap().snapshot_change_count(), Some(2));

    s.outbox_take_snapshot(seq, Snapshot::content(9, 100, 5)).unwrap();
    assert_eq!(s.outbox_row(seq).unwrap().unwrap().snapshot_change_count(), None);
    s.outbox_take_snapshot(seq, sent).unwrap();
    s.outbox_set_snapshot(seq, None).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.snapshot_change_count()), (None, None), "the count goes with the snapshot");
    let has_column = s.conn.query_row("SELECT count(*) FROM pragma_table_info('outbox') WHERE name = 'snapshot_change_count'", [], |row| row.get::<_, i64>(0)).unwrap();
    assert_eq!(has_column, 1);
}
