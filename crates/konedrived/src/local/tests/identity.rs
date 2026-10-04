//! Which object is the item (invariant I2, `Run::resolve`): the one the base
//! records, or the one at the item's place; every other object carrying the
//! id is a copy.

use super::*;

/// I2: an object is the item by the base's record of it, never by the id it
/// carries. A copy that kept the attributes is a copy wherever the original
/// went: out of the folder (the item moved out), or deleted (the item is
/// deleted; the copy is not its rename). A downloaded copy is a new file;
/// one that is not downloaded is listed, and is not the item either.
#[test]
fn a_copy_is_never_the_item_wherever_its_original_went() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"abc"), file("B", "R", "b.txt", b"xyz"), file("P", "R", "p.bin", b"only in the cloud")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("b.txt", b"xyz");
    let original = fx.handle("a.txt");
    for (from, to) in [("a.txt", "a2.txt"), ("b.txt", "b2.txt"), ("p.bin", "p2.bin")] {
        copy_keeping_attributes(&fx.path(from), &fx.path(to));
    }
    std::fs::rename(fx.path("a.txt"), fx.outside.join("a.txt")).unwrap();
    fx.liveness.alive(original.clone(), fx.outside.join("a.txt"));
    // Marked as not downloaded, and holding data all the same.
    fx.write("p2.bin", &[7u8; 8192]);
    std::fs::remove_file(fx.path("b.txt")).unwrap();
    std::fs::remove_file(fx.path("p.bin")).unwrap();
    fx.examine(&names(&[("", "a.txt"), ("", "a2.txt"), ("", "b.txt"), ("", "b2.txt"), ("", "p.bin"), ("", "p2.bin")]));
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (Create, "a2.txt".into(), None),
            (Create, "b2.txt".into(), None),
            (Delete, "b.txt".into(), Some("B".into())),
            (Delete, "p.bin".into(), Some("P".into())),
            (MoveOut, "a.txt".into(), Some("A".into())),
        ]
    );
    assert_eq!((id_of(&fx.path("a2.txt")), id_of(&fx.path("b2.txt"))), (None, None));
    assert_eq!(fx.store.call_blocking(move |s| s.local_handle("A")).unwrap(), Some(original), "the item is still the original");
    let skipped: Vec<(String, String)> =
        fx.store.call_blocking(move |s| s.local_skipped()).unwrap().into_iter().map(|s| (s.rel.display().to_string(), s.reason.to_string())).collect();
    assert_eq!(skipped, vec![("p2.bin".into(), "not-downloaded".into())]);
}

/// An object marked as not downloaded that holds no data and is certainly
/// nobody's file is a file with nothing in it: removed, said in the activity
/// log, and nothing is queued for OneDrive. Certainly: the item's own object
/// was seen in the same run. With the item's object somewhere this run did
/// not look, the empty one may be the item's own file, moved; with an id
/// this store does not know, it may be a file moved in from another
/// account's folder, which that account still has to download where it
/// went. Both stay, listed, and nothing is unlinked.
#[test]
fn an_empty_copy_that_is_not_downloaded_is_removed_only_when_it_is_surely_a_copy() {
    let fx = Fx::new(&[folder("D", "R", "docs"), file("P", "R", "p.bin", b"only in the cloud"), file("Q", "R", "q.bin", b"only in the cloud")]);
    fx.rename("q.bin", "docs/q.bin");
    for (name, id) in [("p2.bin", &b"P"[..]), ("other-account.bin", b"OTHER!1"), ("q2.bin", b"Q")] {
        File::create(fx.path(name)).unwrap().set_len(4096).unwrap();
        xattr::set(fx.path(name), XATTR_ITEM_ID, id).unwrap();
        xattr::set(fx.path(name), placeholder::XATTR_STATE, b"online-only").unwrap();
    }
    fx.examine(&names(&[("", "p.bin"), ("", "p2.bin"), ("", "other-account.bin"), ("", "q2.bin")]));
    assert!(!fx.path("p2.bin").exists());
    assert_eq!(id_of(&fx.path("p.bin")).as_deref(), Some("P"), "the item's own placeholder stays");
    assert_eq!(id_of(&fx.path("q2.bin")).as_deref(), Some("Q"), "not surely a copy: kept");
    assert_eq!(id_of(&fx.path("other-account.bin")).as_deref(), Some("OTHER!1"), "another account's: left alone");
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    let skipped: Vec<(String, String)> =
        fx.store.call_blocking(move |s| s.local_skipped()).unwrap().into_iter().map(|s| (s.rel.display().to_string(), s.reason.to_string())).collect();
    assert_eq!(skipped, vec![("other-account.bin".into(), "not-downloaded".into()), ("q2.bin".into(), "not-downloaded".into())]);
    let said: Vec<(String, String)> = fx.store.call_blocking(move |s| s.recent_activity(10)).unwrap().into_iter().map(|a| (a.kind, a.detail)).collect();
    assert_eq!(said, vec![("removed".into(), "removed an empty copy of p2.bin: it held no content".into())]);
}

/// A folder deleted and then restored at its place from a backup that kept
/// the attributes, while its delete still waits: new inodes, the files that
/// were not downloaded still not downloaded. What stands at the base place
/// is the item again, and the delete is taken back: nothing is deleted in
/// OneDrive, nothing uploaded twice.
#[test]
fn a_folder_restored_at_its_place_takes_its_pending_delete_back() {
    let fx = Fx::new(&[folder("D", "R", "docs"), file("F", "D", "f.txt", b"ff"), file("P", "D", "p.bin", b"only in the cloud")]);
    fx.hydrate("docs/f.txt", b"ff");
    let backup = fx.outside.join("docs");
    let restore = |from: &Path, to: &Path| {
        std::fs::create_dir(to).unwrap();
        for name in xattr::list(from).unwrap() {
            xattr::set(to, &name, &xattr::get(from, &name).unwrap().unwrap()).unwrap();
        }
        for name in ["f.txt", "p.bin"] {
            copy_keeping_attributes(&from.join(name), &to.join(name));
        }
    };
    restore(&fx.path("docs"), &backup);
    std::fs::remove_dir_all(fx.path("docs")).unwrap();
    fx.examine(&names(&[("", "docs")]));
    assert_eq!(fx.summary(), vec![(Delete, "docs".into(), Some("D".into()))]);

    restore(&backup, &fx.path("docs"));
    fx.examine(&names(&[("", "docs")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    for (id, rel) in [("D", "docs"), ("F", "docs/f.txt"), ("P", "docs/p.bin")] {
        assert_eq!(id_of(&fx.path(rel)).as_deref(), Some(id), "{rel}");
        assert_eq!(fx.store.call_blocking(move |s| s.local_handle(id)).unwrap(), Some(fx.handle(rel)), "{rel}");
    }
    assert!(fx.store.call_blocking(move |s| s.local_skipped()).unwrap().is_empty());
}

/// What `F53` leaves: a copy at the item's place while the item's object is
/// in a directory the batch does not look at. The copy is taken for the
/// item; the object that moved, seen later, is then the copy. Two files,
/// no delete and no move in OneDrive.
#[test]
fn a_copy_at_the_place_and_the_original_seen_later_are_two_files() {
    let fx = Fx::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"abc")]);
    fx.hydrate("a.txt", b"abc");
    fx.rename("a.txt", "docs/a.txt");
    copy_keeping_attributes(&fx.path("docs/a.txt"), &fx.path("a.txt"));
    fx.examine(&names(&[("", "a.txt")]));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    fx.examine(&names(&[("docs", "a.txt")]));
    assert_eq!(fx.summary(), vec![(Create, "docs/a.txt".into(), None)]);
    assert_eq!((id_of(&fx.path("a.txt")).as_deref(), id_of(&fx.path("docs/a.txt"))), (Some("A"), None));
}

/// I2, issue #113: a copy made back at the item's old place after the item
/// was moved is a new file or folder there; the item stays the object that
/// moved, and nothing takes its move back.
#[test]
fn a_copy_made_back_at_the_old_place_after_a_move_is_new() {
    let fx = Fx::new(&[folder("D", "R", "docs"), file("F", "D", "f.txt", b"ff"), file("A", "R", "a.txt", b"abc")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("docs/f.txt", b"ff");
    fx.rename("a.txt", "b.txt");
    fx.rename("docs", "moved");
    fx.examine(&names(&[("", "a.txt"), ("", "b.txt"), ("", "docs"), ("", "moved")]));
    let moves = vec![(Move, "b.txt".into(), Some("A".into())), (Move, "moved".into(), Some("D".into()))];
    assert_eq!(fx.summary(), moves);
    let (item, dir) = (fx.handle("b.txt"), fx.handle("moved"));

    copy_keeping_attributes(&fx.path("b.txt"), &fx.path("a.txt"));
    std::fs::create_dir(fx.path("docs")).unwrap();
    for name in xattr::list(fx.path("moved")).unwrap() {
        xattr::set(fx.path("docs"), &name, &xattr::get(fx.path("moved"), &name).unwrap().unwrap()).unwrap();
    }
    copy_keeping_attributes(&fx.path("moved/f.txt"), &fx.path("docs/f.txt"));
    fx.examine(&names(&[("", "a.txt"), ("", "docs")]));
    let mut rows = fx.summary();
    rows.sort();
    let mut expected = moves;
    expected.extend([(Create, "a.txt".into(), None), (Create, "docs/f.txt".into(), None), (Mkdir, "docs".into(), None)]);
    expected.sort();
    assert_eq!(rows, expected);
    for (id, rel) in [("A", "b.txt"), ("D", "moved"), ("F", "moved/f.txt")] {
        assert_eq!(id_of(&fx.path(rel)).as_deref(), Some(id), "{rel} is untouched");
    }
    assert_eq!(fx.store.call_blocking(move |s| Ok((s.local_handle("A")?, s.local_handle("D")?))).unwrap(), (Some(item), Some(dir)));
}

/// I2, issue #113: an item the base does not place (its name cannot be
/// placed here) has no object on disk. A copy carrying its id — of the
/// folder as it was, kept with its attributes — is the user's own: uploaded
/// as new, never a rename of the item in OneDrive.
#[test]
fn what_carries_the_id_of_an_item_the_base_does_not_place_is_new() {
    let mut long = row("D", Some("R"), "docs", Kind::Folder, b"");
    long.placement = Placement::Skipped(konedrive_tree::SkipReason::NameTooLong);
    let fx = Fx::new(&[Change::Upsert(long), file("F", "D", "f.txt", b"ff")]);
    assert!(!fx.path("docs").exists());
    std::fs::create_dir(fx.path("copy")).unwrap();
    xattr::set(fx.path("copy"), XATTR_ITEM_ID, b"D").unwrap();
    fx.write("copy/f.txt", b"ff");
    xattr::set(fx.path("copy/f.txt"), XATTR_ITEM_ID, b"F").unwrap();
    xattr::set(fx.path("copy/f.txt"), placeholder::XATTR_STATE, b"hydrated").unwrap();
    let out = fx.examine(&names(&[("", "copy")]));
    fx.examine(&out.recheck);
    let mut rows = fx.summary();
    rows.sort();
    assert_eq!(rows, vec![(Create, "copy/f.txt".into(), None), (Mkdir, "copy".into(), None)]);
    assert_eq!((id_of(&fx.path("copy")), id_of(&fx.path("copy/f.txt"))), (None, None));
}

/// I2 with no object recorded (a rebuilt store): the object standing where
/// the base places the item is the item, and is recorded again. One the user
/// moved meanwhile is not taken for a move: it is uploaded as new, and the
/// item, never deleted, is left for the reconcile to place again.
#[test]
fn with_no_recorded_object_the_item_is_only_what_stands_at_its_place() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"abc"), file("B", "R", "b.txt", b"xyz")]);
    fx.hydrate("b.txt", b"xyz");
    fx.store.call_blocking(move |s| s.forget_local_handles()).unwrap();
    fx.rename("b.txt", "moved.txt");
    let out = fx.examine(&Batch::full());
    assert_eq!(fx.summary(), vec![(Create, "moved.txt".into(), None)]);
    assert_eq!(out.unproven, vec!["B".to_owned()]);
    assert_eq!(id_of(&fx.path("moved.txt")), None);
    assert_eq!(fx.store.call_blocking(move |s| Ok((s.local_handle("A")?, s.local_handle("B")?))).unwrap(), (Some(fx.handle("a.txt")), None));
}
