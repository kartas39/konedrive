use super::*;
use crate::outbox::{Detection, OutboxKind, OutboxOp, OutboxState, Snapshot};
use crate::TreeStore;

/// Every reason is written as it was read, byte for byte: with a detail,
/// with sizes, and what no variant spells.
#[test]
fn a_stored_reason_is_written_back_as_it_was_read() {
    let typed = [
        ("refused: The name is not allowed", Reason::Refused(Some("The name is not allowed".into()))),
        ("refused: ", Reason::Refused(Some(String::new()))),
        ("refused", Reason::Refused(None)),
        ("not allowed now: the folder is read-only: for now", Reason::NotAllowed(Some("the folder is read-only: for now".into()))),
        ("download-failed: errno 5", Reason::Download(Some("errno 5".into()))),
        ("moved-out-unreachable: errno 13", Reason::Unreachable(Some("errno 13".into()))),
        ("too-big:300:20", Reason::TooBig(Some((300, 20)))),
        ("too-big", Reason::TooBig(None)),
        ("gone-once", Reason::GoneOnce),
    ];
    for (stored, reason) in typed {
        assert_eq!(Reason::parse(stored), reason, "{stored}");
        assert_eq!(reason.to_string(), stored);
    }
    // No variant spells these: a key that takes no detail with one behind it, sizes
    // not written as the daemon writes them, an older version's text, nothing at all.
    for stored in ["network: connection reset", "too-big:0300:20", "too-big:x", "hash-mismatch:01ABC", "unreadable kind \"frobnicate\"", "error sending request", ""] {
        let reason = Reason::parse(stored);
        assert_eq!(reason, Reason::Other(stored.to_owned()), "{stored}");
        assert_eq!(reason.to_string(), stored);
    }
}

/// The key, the group and the detail of what no variant spells are what
/// the string says.
#[test]
fn what_no_variant_spells_still_has_its_key_and_group() {
    let of = |stored: &str| {
        let reason = Reason::parse(stored);
        (reason.key().to_owned(), reason.group(), reason.detail())
    };
    assert_eq!(of("network: connection reset"), ("network".into(), Some(Group::Waiting), Some("connection reset".into())));
    assert_eq!(of("too-big:0300:20"), ("too-big".into(), Some(Group::OneAction), Some("0300:20".into())));
    assert_eq!(of("symlink: odd"), ("symlink".into(), Some(Group::Never), Some("odd".into())));
    assert_eq!(of("mass-delete: odd"), ("mass-delete: odd".into(), None, None));
    assert_eq!(of("something-new"), ("something-new".into(), None, None));
    assert!(Reason::parse("too-big:0300:20").waits_for_space() && Reason::parse("too-big:x").waits_for_space());
    assert_eq!(Reason::parse("too-big:0300:20").sizes(), Some((300, 20)));
    assert!(!Reason::TooBig(None).waits_for_space() && !Reason::Quota.waits_for_space());
    assert!(Reason::parse("").is_empty() && !Reason::Blocked.is_empty());
}

/// The key of a variant is the key its stored string is summed under, and
/// the two tables share no key but `not-downloaded`, which is grouped the same in both.
#[test]
fn a_variants_key_is_the_key_of_its_stored_string() {
    for reason in Reason::ALL {
        assert_eq!(key_of(&reason.to_string()), reason.key());
        assert_eq!(Reason::from_key(reason.key()), Some(reason.clone()));
        assert_eq!(known_group(reason.key()), reason.group(), "{reason}");
        assert_eq!(reason.detail(), None, "{reason}");
    }
    for skip in LocalSkip::ALL {
        assert_eq!(LocalSkip::parse(skip.key()), skip);
        assert_eq!((skip.to_string().as_str(), skip.detail()), (skip.key(), None));
        assert_eq!(known_group(skip.key()), skip.group(), "{skip}");
        assert_eq!(Reason::from_key(skip.key()).is_some(), skip == LocalSkip::NotDownloaded, "{skip}");
    }
    assert_eq!(LocalSkip::parse("something-new"), LocalSkip::Other("something-new".into()));
    for group in Group::ALL {
        assert_eq!(Group::parse(group.as_str()), Some(group));
    }
}

/// A reason no variant spells, in the database, is read, kept through a
/// change of the row's state, counted and listed, as stored.
#[test]
fn a_row_with_a_reason_the_enum_does_not_know_is_kept() {
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
        state: OutboxState::Retry,
        reason: None,
        next_try: None,
        size: None,
    };
    s.outbox_apply(&[OutboxOp::Record(d), OutboxOp::Skip { rel: "odd".into(), reason: LocalSkip::Symlink, size: 0 }], 1).unwrap();
    s.conn.execute("UPDATE outbox SET reason = 'error sending request for url'", []).unwrap();
    s.conn.execute("UPDATE local_skipped SET reason = 'from-the-future'", []).unwrap();
    let row = s.outbox_rows().unwrap().remove(0);
    assert_eq!(row.reason, Some(Reason::Other("error sending request for url".into())));
    s.outbox_set_state(row.seq, OutboxState::Retry, row.reason.as_ref(), Some(7)).unwrap();
    let stored: String = s.conn.query_row("SELECT reason FROM outbox", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, "error sending request for url");
    assert_eq!(s.outbox_groups().unwrap()[0].reason(), row.reason);
    assert_eq!(s.local_skipped().unwrap()[0].reason, LocalSkip::Other("from-the-future".into()));
    let groups = s.skipped_groups().unwrap();
    assert_eq!(s.skipped_places_of(&[&groups[0].reason], 0).unwrap(), vec![("odd".into(), LocalSkip::Other("from-the-future".into()))]);
}

/// The rows that wait for space are found by the two spellings.
#[test]
fn the_rows_waiting_for_space_are_found_by_their_reasons() {
    let mut s = TreeStore::in_memory().unwrap();
    let create = |rel: &str, reason: Option<Reason>| {
        OutboxOp::Record(Detection {
            kind: OutboxKind::Create,
            item_id: None,
            inode: None,
            rel: rel.into(),
            base: None,
            target_parent: None,
            target_name: Some(rel.into()),
            same_content: false,
            state: OutboxState::Ready,
            reason,
            next_try: None,
            size: None,
        })
    };
    let ops = [create("a", Some(Reason::WaitingForSpace)), create("b", Some(Reason::TooBig(Some((9, 1))))), create("c", Some(Reason::Quota)), create("d", None)];
    s.outbox_apply(&ops, 1).unwrap();
    let waiting: Vec<String> = s.outbox_waiting_for_space().unwrap().into_iter().map(|r| r.rel.display().to_string()).collect();
    assert_eq!(waiting, ["a", "b"]);
    let stored: Vec<String> =
        s.conn.prepare("SELECT reason FROM outbox WHERE reason IS NOT NULL ORDER BY seq").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(stored, ["waiting-for-space", "too-big:9:1", "quota-exceeded"]);
}

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

/// The group of every key, written out: what `NotUploadedSummary()` lists
/// each reason under.
#[test]
fn every_key_has_the_group_it_had() {
    let table: [(Option<Group>, &[&str]); 5] = [
        (Some(Group::OneAction), &["quota-exceeded", "waiting-for-space", "too-big", "forbidden"]),
        (
            Some(Group::PerFile),
            &[
                "name-characters",
                "name-spaces",
                "name-reserved",
                "name-not-utf8",
                "too-large",
                "refused",
                "unknown-state",
                "mounted-inside",
                "leaving-not-found",
                "no-name",
                "no-item",
                "no-guard",
                "no-handle",
                "bad-handle",
                "another-item",
                "state-unreadable",
                "blocked",
            ],
        ),
        (Some(Group::Never), &["symlink", "fifo", "socket", "device", "other-device", "reserved-name", "hard-link", "ignored"]),
        (
            Some(Group::Waiting),
            &[
                "open-for-writing",
                "locked",
                "not-found",
                "not-downloaded",
                "changed-while-sending",
                "parent-not-in-onedrive",
                "hash-mismatch",
                "move-out-not-yet",
                "waiting-for-the-helper",
                "moved-out-unreachable",
                "back-in-the-folder",
                "moved-out-place-unknown",
                "download-failed",
                "gone-once",
                "handle-from-another-filesystem",
                "gone-unproved",
                "lease-probe-failed",
                "network",
                "local-error",
                "index-error",
                "upload-error",
                "moved-out-not-opened",
                "paused",
                "upload-session-open",
                "name-held-by-an-upload",
                "changed in OneDrive again and again",
                "changing in OneDrive again and again",
                "the upload session ended twice",
                "not allowed now",
            ],
        ),
        (None, &["mass-delete", "something-new"]),
    ];
    let mut keys = 0;
    for (group, of) in table {
        for key in of {
            assert_eq!(known_group(key), group, "{key}");
            keys += 1;
        }
    }
    // Every key of the two tables is above: `not-downloaded` is in both, `something-new` in neither.
    assert_eq!(keys, Reason::ALL.len() + LocalSkip::ALL.len());
}
