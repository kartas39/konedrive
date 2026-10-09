//! A directory that carries the id of another directory of OneDrive (a copy that kept its
//! attributes): nothing new or moved is sent into the directory that id names for a row
//! that names no directory itself.

use super::*;
use konedrive_tree::outbox::{Base, Detection};

/// `Copy/`, carrying the id of the folder `d` (item `D`), as a copy that kept
/// its attributes does.
fn copy_of_d(w: &World) {
    std::fs::create_dir(w.path("Copy")).unwrap();
    xattr::set(w.path("Copy"), XATTR_ITEM_ID, b"D").unwrap();
}

fn writes(w: &World) -> Vec<(String, String)> {
    w.cloud(|c| c.log.iter().filter(|(method, _)| method != "GET").cloned().collect())
}

/// A new directory and its file below the copy, recorded with no directory of OneDrive to
/// go into. The worker does not take `Copy`'s id for one: nothing is made inside the real
/// `d`, and the rows wait. Once the copy is a local-only directory of its own, with its row,
/// everything goes up under it.
#[test]
fn a_new_folder_below_a_copy_of_a_folder_is_not_made_in_the_original() {
    let w = World::new(&[folder("D", "R", "d")]);
    copy_of_d(&w);
    std::fs::create_dir(w.path("Copy/sub")).unwrap();
    w.write("Copy/sub/new.txt", b"new");
    let sub = Detection { target_parent: None, ..w.detection(Mkdir, Path::new("Copy/sub"), Some(w.handle("Copy/sub")), None) };
    let new = w.detection(Create, Path::new("Copy/sub/new.txt"), Some(w.handle("Copy/sub/new.txt")), Some(3));
    w.record(vec![sub, new], Vec::new());
    assert_eq!(w.summary(), vec![(Mkdir, "Copy/sub".into(), OutboxState::Ready), (Create, "Copy/sub/new.txt".into(), OutboxState::Ready)]);
    assert_eq!((w.rows()[0].target_parent.clone(), w.rows()[1].target_parent.clone()), (None, None));

    w.run();
    assert!(writes(&w).is_empty(), "{:?}", writes(&w));
    assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
    assert_eq!(reason_of(&w, "Copy/sub").as_deref(), Some(Reason::Parent.key()));
    assert_eq!(w.rows().len(), 2, "both wait: {:?}", w.summary());

    // The copy loses the id it carried and is recorded as the new directory it is (the
    // rows' wait over).
    xattr::remove(w.path("Copy"), XATTR_ITEM_ID).unwrap();
    w.record(vec![w.detection(Mkdir, Path::new("Copy"), Some(w.handle("Copy")), None)], Vec::new());
    for row in w.rows() {
        w.store.call_blocking(move |s| s.outbox_set_state(row.seq, OutboxState::Ready, None, None)).unwrap();
    }
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["Copy", "Copy/sub", "Copy/sub/new.txt", "d"]);
}

/// A file of OneDrive moved into the copy, its `move` recorded with no directory of
/// OneDrive to go into: it is not moved into the real `d`.
#[test]
fn an_item_moved_into_a_copy_of_a_folder_is_not_moved_into_the_original() {
    let w = World::new(&[folder("D", "R", "d"), file("A", "R", "a.txt", b"a")]);
    copy_of_d(&w);
    w.download("a.txt", b"a");
    std::fs::rename(w.path("a.txt"), w.path("Copy/a.txt")).unwrap();
    let base = w.base("A").unwrap();
    let moved = Detection {
        item_id: Some("A".into()),
        base: Some(Base { etag: base.etag.clone(), ctag: base.ctag.clone(), parent: base.parent_id.clone(), name: Some(base.name.clone()) }),
        target_parent: None,
        same_content: true,
        ..w.detection(Move, Path::new("Copy/a.txt"), Some(w.handle("Copy/a.txt")), None)
    };
    w.record(vec![moved], Vec::new());

    w.run();
    assert!(writes(&w).is_empty(), "{:?}", writes(&w));
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt", "d"]);
    assert_eq!(reason_of(&w, "Copy/a.txt").as_deref(), Some(Reason::Parent.key()));
}
