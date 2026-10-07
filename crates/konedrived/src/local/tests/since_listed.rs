//! What the user does between the moment a run lists an entry and the moment
//! it acts on it: the run acts on the object it listed, never on whatever
//! stands at that name by then.

use super::passed_over::scan_changing;
use super::*;

/// A copy that kept the item's marks is listed, and before the run takes
/// its marks off the user renames the item's own file over it. The name
/// holds the item's object now: it keeps its marks, nothing goes up as new,
/// and the next look at the two names finds the item renamed.
#[test]
fn a_name_that_holds_another_object_by_the_time_it_is_stripped_is_left_alone() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"abc")]);
    fx.hydrate("a.txt", b"abc");
    let item = fx.handle("a.txt");
    copy_keeping_attributes(&fx.path("a.txt"), &fx.path("b.txt"));
    let out = scan_changing(&fx, |_| {
        if fx.path("a.txt").exists() {
            fx.rename("a.txt", "b.txt");
        }
    })
    .unwrap();
    assert_eq!(id_of(&fx.path("b.txt")).as_deref(), Some("A"), "the item's own file keeps its marks");
    assert!(out.stripped.is_empty(), "{:?}", out.stripped);
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    assert!(!out.recheck.is_empty(), "the name is looked at again");

    fx.examine(&names(&[("", "a.txt"), ("", "b.txt")]));
    assert_eq!(fx.summary(), vec![(Move, "b.txt".into(), Some("A".into()))]);
    assert_eq!(fx.handle("b.txt"), item);
}

/// The same for an editor's backup (the item's old inode under an ignored
/// name, a new file at the item's name): the backup's name holds another
/// object by the time its marks would come off, and that object is left
/// as it is.
#[test]
fn a_backup_whose_name_holds_another_object_by_then_is_left_alone() {
    let fx = Folder::new(&[file("A", "R", "a.txt", b"abc"), file("B", "R", "b.txt", b"xyz")]);
    fx.hydrate("a.txt", b"abc");
    fx.hydrate("b.txt", b"xyz");
    fx.rename("a.txt", "a.txt~");
    fx.write("a.txt", b"the new text");
    scan_changing(&fx, |_| {
        if fx.path("b.txt").exists() {
            fx.rename("b.txt", "a.txt~");
        }
    })
    .unwrap();
    assert_eq!(id_of(&fx.path("a.txt~")).as_deref(), Some("B"), "another item's file, renamed over the backup, keeps its marks");
}

/// A new file that is gone again before the run looks whether a program
/// has it open gets no row; one replaced by another new file gets its row
/// when its name is looked at again, for the object that is there.
#[test]
fn a_new_file_that_went_since_it_was_listed_gets_no_row() {
    let fx = Folder::new(&[]);
    fx.write("gone.txt", b"n");
    fx.write("other.txt", b"first");
    let first = fx.handle("other.txt");
    let out = scan_changing(&fx, |_| {
        if fx.path("gone.txt").exists() {
            std::fs::remove_file(fx.path("gone.txt")).unwrap();
            std::fs::write(fx.path("other.txt.new"), b"second").unwrap();
            fx.rename("other.txt.new", "other.txt");
        }
    })
    .unwrap();
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());

    fx.examine(&out.recheck);
    assert_eq!(fx.summary(), vec![(Create, "other.txt".into(), None)]);
    let row = fx.row_at("other.txt");
    assert_ne!(row.inode.and_then(|i| i.handle), Some(first), "the row is of the file that is there");
    assert_eq!(row.size, Some(6));
}
