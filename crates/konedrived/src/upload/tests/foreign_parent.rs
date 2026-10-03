//! A directory that carries the id of another folder (a copy that kept its
//! attributes and was not stripped: `LO3`): nothing new or moved is sent into
//! the folder that id names, whichever run recorded the row.

use super::*;
use konedrive_tree::outbox::{Base, Detection, Inode};

/// `Copy/`, carrying the id of the folder `d` (item `D`), as a copy that kept
/// its attributes does.
fn copy_of_d(w: &World) {
    std::fs::create_dir(w.path("Copy")).unwrap();
    xattr::set(w.path("Copy"), XATTR_ITEM_ID, b"D").unwrap();
}

fn writes(w: &World) -> Vec<(String, String)> {
    w.cloud(|c| c.log.iter().filter(|(method, _)| method != "GET").cloned().collect())
}

/// Two levels below the copy: the batch names only `Copy/sub`, so the
/// examination never judges `Copy` in that run, and records the new folder
/// and its file with no parent. The worker does not take `Copy`'s id for one:
/// nothing is made inside the real `d`, and the rows wait.
#[test]
fn a_new_folder_below_a_copy_of_a_folder_is_not_made_in_the_original() {
    let w = World::new(&[folder("D", "R", "d")]);
    copy_of_d(&w);
    std::fs::create_dir(w.path("Copy/sub")).unwrap();
    w.write("Copy/sub/new.txt", b"new");
    w.examine(&[("Copy/sub", "new.txt")]);
    assert_eq!(
        w.summary(),
        vec![(Mkdir, "Copy/sub".into(), OutboxState::Ready), (Create, "Copy/sub/new.txt".into(), OutboxState::Ready)],
        "the batch does not see that `Copy` is a copy"
    );
    assert_eq!(w.rows()[0].target_parent, None);

    w.run();
    assert!(writes(&w).is_empty(), "{:?}", writes(&w));
    assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
    assert_eq!(reason_of(&w, "Copy/sub").as_deref(), Some(reason::PARENT));
    assert_eq!(w.rows().len(), 2, "both wait: {:?}", w.summary());

    // The copy examined and stripped, it is a new folder, and everything goes up under it
    // (the rows' wait over).
    w.examine(&[("", "Copy")]);
    for row in w.rows() {
        w.store.call_blocking(move |s| s.outbox_set_state(row.seq, OutboxState::Ready, None, None)).unwrap();
    }
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["Copy", "Copy/sub", "Copy/sub/new.txt", "d"]);
}

/// An item moved into the copy, its `move` recorded with no parent (a run
/// that could not strip the copy records it so): it is not moved into the
/// real `d`.
#[test]
fn an_item_moved_into_a_copy_of_a_folder_is_not_moved_into_the_original() {
    let w = World::new(&[folder("D", "R", "d"), file("A", "R", "a.txt", b"a")]);
    copy_of_d(&w);
    w.rename("a.txt", "Copy/a.txt");
    let meta = std::fs::metadata(w.path("Copy/a.txt")).unwrap();
    let base = w.base("A").unwrap();
    let moved = Detection {
        kind: Move,
        item_id: Some("A".into()),
        inode: Some(Inode { dev: meta.dev(), ino: meta.ino(), handle: Some(w.handle("Copy/a.txt")) }),
        rel: "Copy/a.txt".into(),
        base: Some(Base { etag: base.etag.clone(), ctag: base.ctag.clone(), parent: base.parent_id.clone(), name: Some(base.name.clone()) }),
        target_parent: None,
        target_name: Some("a.txt".into()),
        same_content: true,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: Some(1),
    };
    w.store.call_blocking(move |s| s.outbox_record(&moved)).unwrap();

    w.run();
    assert!(writes(&w).is_empty(), "{:?}", writes(&w));
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt", "d"]);
    assert_eq!(reason_of(&w, "Copy/a.txt").as_deref(), Some(reason::PARENT));
}

/// A folder that is leaving and was placed again elsewhere meanwhile (issue
/// #104): the base records the new copy's object, `leaving` the old one's.
/// What waits in the old one, and in a folder inside it, still goes up into
/// their items, so that nothing keeps the old one on disk.
#[test]
fn what_waits_in_a_leaving_folder_placed_again_elsewhere_goes_into_its_item() {
    let w = World::new(&[folder("D", "R", "d"), folder("S", "D", "sub")]);
    let (old, old_sub) = (w.handle("d"), w.handle("d/sub"));
    std::fs::create_dir_all(w.path("again/sub")).unwrap();
    let (new, new_sub) = (w.handle("again"), w.handle("again/sub"));
    w.store
        .call_blocking(move |s| {
            s.leaving_add("D", Path::new("d"), Some(&old))?;
            s.set_local_handle("D", Some(&new))?;
            s.set_local_handle("S", Some(&new_sub))
        })
        .unwrap();
    assert_ne!(old_sub, w.handle("again/sub"));
    for rel in ["d/new.txt", "d/sub/deep.txt"] {
        w.write(rel, b"new");
        let meta = std::fs::metadata(w.path(rel)).unwrap();
        let create = Detection {
            kind: Create,
            item_id: None,
            inode: Some(Inode { dev: meta.dev(), ino: meta.ino(), handle: Some(w.handle(rel)) }),
            rel: rel.into(),
            base: None,
            target_parent: None,
            target_name: Some(rel.rsplit('/').next().unwrap().into()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: Some(3),
        };
        w.store.call_blocking(move |s| s.outbox_record(&create)).unwrap();
    }

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/new.txt", "d/sub", "d/sub/deep.txt"]);
}
