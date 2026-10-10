//! A store that was lost and made again: the rows are made from the disk, a marked entry
//! bound to the id of its mark, an unmarked one new.

use konedrive_tree::{ContentPresence, ContentVersion, ItemKind};

use super::{changed_here, content, folder, found, reference, DiskFinding, Folder, MTIME};

/// A backing directory as a daemon left it, and no row but the root's.
fn tree_with_no_rows() -> Folder {
    let f = folder();
    f.make_directory("docs");
    f.mark_directory("docs", "D");
    f.write("docs/a.txt", b"aaa");
    f.mark_copy("docs/a.txt", "A");
    f.make_directory("docs/sub");
    f.mark_directory("docs/sub", "S");
    f.write("docs/sub/b.txt", b"bbbb");
    f.mark_copy("docs/sub/b.txt", "B");
    f.make_directory("mine");
    f.write("mine/own.txt", b"own");
    f.write("top.txt", b"top");
    f
}

#[test]
fn every_marked_entry_is_bound_to_the_id_of_its_mark_and_an_unmarked_one_is_new() {
    let mut f = tree_with_no_rows();
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::Made, 3), (DiskFinding::MarkedWithoutRow, 4)]);
    assert_eq!(findings.unresolved(), []);
    for (path, id) in [("docs", "D"), ("docs/a.txt", "A"), ("docs/sub", "S"), ("docs/sub/b.txt", "B")] {
        assert_eq!(f.row(path).unwrap().onedrive_item, Some(reference("d1", id)), "{path}");
        assert_eq!(f.mark_of(path).as_deref(), Some(id), "{path}: the mark stays");
    }
    for path in ["mine", "mine/own.txt", "top.txt"] {
        assert_eq!(f.row(path).unwrap().onedrive_item, None, "{path}");
    }
    // A whole copy is taken with the content its cTag mark tells, with no hash: the plan
    // compares it with what a listing brings. Nothing is agreed on yet.
    let a = f.row("docs/a.txt").unwrap();
    assert_eq!(content(&a), Some(ContentVersion::Hashed { size: 3, mtime: MTIME, mtime_nsec: 0, hash: None, ctag: Some("c-A".to_owned()) }));
    assert_eq!((a.content_presence, a.synced_place.clone(), a.synced_content.clone(), a.onedrive.clone()), (ContentPresence::OnDisk, None, None, None));
    assert_eq!(content(&f.row("top.txt").unwrap()), changed_here(3, 1));
    f.assert_settled();
}

/// Review 3, M1's second half: a file that carries the id alone holds the person's bytes.
#[test]
fn a_file_marked_with_an_id_alone_is_bound_with_its_bytes_as_content_changed_here() {
    let mut f = folder();
    f.write("saved.txt", b"saved by rename");
    xattr::set(f.disk("saved.txt"), konedrive_fs::placeholder::XATTR_ITEM_ID, b"A").unwrap();
    let findings = f.check();
    assert_eq!((found(&findings), findings.unresolved()), (vec![(DiskFinding::MarkedWithoutRow, 1)], &[][..]));
    let row = f.row("saved.txt").unwrap();
    assert_eq!((row.onedrive_item.clone(), content(&row), row.content_presence), (Some(reference("d1", "A")), changed_here(15, 1), ContentPresence::OnDisk));
    f.assert_settled();
}

#[test]
fn a_marked_entry_no_row_has_is_bound_in_the_drive_of_the_nearest_directory_of_onedrive_above_it() {
    let mut f = folder();
    f.directory_of_drive("shared", "d2", "S");
    f.make_directory("shared/inner");
    f.write("shared/inner/x.bin", b"x");
    f.mark_copy("shared/inner/x.bin", "X");
    f.write("y.bin", b"y");
    f.mark_copy("y.bin", "Y");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 1), (DiskFinding::Made, 1), (DiskFinding::MarkedWithoutRow, 2)]);
    assert_eq!(f.row("shared/inner/x.bin").unwrap().onedrive_item, Some(reference("d2", "X")));
    assert_eq!(f.row("y.bin").unwrap().onedrive_item, Some(reference("d1", "Y")));
}

/// Two entries carry one id that no row has: the first that is gone through, a directory
/// before what is in it and by name, is bound; the other is the user's own.
#[test]
fn of_two_entries_marked_with_one_id_no_row_has_one_is_bound_and_the_other_is_new() {
    let mut f = folder();
    f.make_directory("copies");
    f.write("copies/a.txt", b"aaa");
    f.mark_copy("copies/a.txt", "A");
    f.write("a.txt", b"aaa");
    f.mark_copy("a.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 2), (DiskFinding::MarkedWithoutRow, 1)]);
    assert_eq!(f.row("a.txt").unwrap().onedrive_item, Some(reference("d1", "A")));
    assert_eq!((f.row("copies/a.txt").unwrap().onedrive_item, f.mark_of("copies/a.txt")), (None, None));
    assert_eq!(f.row("copies").unwrap().kind, ItemKind::Directory);
    f.assert_settled();
}

/// The id is one a row has that the folder does not show: no row is bound to it again.
#[test]
fn an_entry_marked_with_an_id_a_row_the_folder_does_not_show_has_is_new() {
    let mut f = folder();
    let listed = f.store.item_store().make_onedrive_item(&reference("d1", "A"), ItemKind::File).unwrap();
    f.write("a.txt", b"aaa");
    f.mark_copy("a.txt", "A");
    let before = f.item(listed);
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1)]);
    assert_eq!((f.row("a.txt").unwrap().onedrive_item, f.mark_of("a.txt")), (None, None));
    assert_eq!(f.item(listed), before);
}
