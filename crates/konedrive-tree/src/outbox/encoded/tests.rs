use crate::outbox::{Detection, OutboxKind, OutboxOp, OutboxState, Snapshot};
use crate::TreeStore;

/// A row's snapshot and the forms of its target name, over the strings the
/// columns hold.
#[test]
fn a_rows_snapshot_and_target_name_are_read_as_stored() {
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
    let snapshot = |s: &TreeStore| -> Option<String> { s.conn.query_row("SELECT snapshot FROM outbox", [], |r| r.get(0)).unwrap() };

    let content = Snapshot::content(100, 2, 5);
    s.outbox_take_snapshot(seq, content).unwrap();
    assert_eq!(snapshot(&s).as_deref(), Some("100 2000000005"));
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.snapshot_size(), row.snapshot_sent()), (Some(content), Some(100), Some((100, 2))));
    assert!(row.snapshot_is(content) && !row.snapshot_is(Snapshot::content(100, 2, 6)));
    assert_eq!(Snapshot::content(1, -1, 5).to_string(), "1 -999999995", "a time before 1970");
    assert_eq!(Snapshot::parse("1 -999999995"), Some(Snapshot::content(1, -1, 5)));

    for (marker, stored) in [(Snapshot::ContentLocal, "moved-out:local"), (Snapshot::Trashed, "moved-out:trash")] {
        s.outbox_set_snapshot(seq, Some(marker)).unwrap();
        assert_eq!(snapshot(&s).as_deref(), Some(stored));
        let row = s.outbox_row(seq).unwrap().unwrap();
        assert_eq!((row.snapshot(), row.snapshot_size(), row.snapshot_sent()), (Some(marker), None, None));
        assert!(marker.is_marker() && row.snapshot_is(marker));
    }
    // What is none of these is no snapshot; a size before a space is still a size.
    s.outbox_amend(seq, |row| row.snapshot = Some("12 soon".into())).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.snapshot(), row.snapshot_size(), row.snapshot_sent()), (None, Some(12), None));
    s.outbox_set_snapshot(seq, None).unwrap();
    assert_eq!(snapshot(&s), None);

    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (None, None), "a name");
    s.outbox_set_target(seq, Some("R"), Some(".konedrive-swap-7")).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (Some(".konedrive-swap-7"), None));
    s.outbox_set_target(seq, None, crate::outbox::place_name(std::path::Path::new("/home/u/a.txt"))).unwrap();
    let row = s.outbox_row(seq).unwrap().unwrap();
    assert_eq!((row.swap_name(), row.last_place()), (None, Some(std::path::Path::new("/home/u/a.txt"))));
}
