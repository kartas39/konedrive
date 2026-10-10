//! What a person moves by hand while no daemon runs: renames, moves over other items,
//! exchanges, and copies that keep the marks.

use std::fs;

use konedrive_tree::{ContentPresence, ItemKind, LocalVersionChange};

use super::{changed_here, content, folder, folder_of_each, found, reference, DiskFinding};

#[test]
fn a_file_renamed_by_hand_is_the_same_item_at_its_new_place() {
    let mut f = folder_of_each();
    let (a, before) = (f.id("docs/a.txt"), f.rows());
    f.rename("docs/a.txt", "renamed.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1)]);
    assert_eq!((f.id("renamed.txt"), f.row("docs/a.txt")), (a, None));
    let changed: Vec<_> = f.rows().into_iter().zip(before).filter(|(now, was)| now != was).map(|(now, _)| now.local_item_id).collect();
    assert_eq!(changed, [a], "one row is written");
    assert_eq!(f.item(a).synced_place.unwrap().name, "a.txt", "what was agreed on stays: the move is to be sent");
    f.assert_settled();
}

#[test]
fn an_item_moved_into_a_directory_made_by_hand_follows_it() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.make_directory("new");
    f.make_directory("new/deeper");
    f.rename("docs/a.txt", "new/deeper/a.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 2), (DiskFinding::Moved, 1)]);
    assert_eq!(f.id("new/deeper/a.txt"), a);
    f.assert_settled();
}

/// The rows below a moved directory name it by its id: they are found where they are, and
/// are not a thousand moves.
#[test]
fn a_directory_moved_by_hand_with_a_thousand_files_below_writes_one_row() {
    let mut f = folder();
    f.onedrive_directory("from", "F");
    f.onedrive_directory("to", "T");
    let moved = f.onedrive_directory("from/dir", "D");
    f.own_directory("from/dir/own");
    for n in 0..500 {
        f.copy(&format!("from/dir/copy-{n}"), b"c", &format!("C{n}"));
        f.own_file(&format!("from/dir/own/file-{n}"), b"o");
    }
    let before = f.rows();
    f.rename("from/dir", "to/dir");
    let findings = f.check();
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 1003), (DiskFinding::Moved, 1)]);
    let changed: Vec<_> = f.rows().into_iter().zip(before).filter(|(now, was)| now != was).map(|(now, _)| now.local_item_id).collect();
    assert_eq!(changed, [moved], "one row is written");
    assert_eq!(f.id("to/dir"), moved);
    assert!(f.row("to/dir/copy-499").is_some() && f.row("to/dir/own/file-499").is_some() && f.row("from/dir").is_none());
    f.assert_settled();
}

#[test]
fn a_directory_moved_and_a_file_in_it_edited_are_a_move_and_a_change_of_content() {
    let mut f = folder_of_each();
    let (docs, a) = (f.id("docs"), f.id("docs/a.txt"));
    f.rename("docs", "papers");
    f.write("papers/a.txt", b"edited by hand");
    f.mark_copy("papers/a.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::Moved, 1), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.id("papers"), f.id("papers/a.txt")), (docs, a));
    assert_eq!(content(&f.item(a)), changed_here(14, 1));
    assert!(f.row("papers/remote.bin").is_some(), "what is not on disk goes with its directory");
    f.assert_settled();
}

#[test]
fn a_file_moved_and_edited_is_both() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.rename("docs/a.txt", "a.txt");
    fs::write(f.disk("a.txt"), b"moved, then edited").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 1), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.id("a.txt"), content(&f.item(a)).map(|content| content.size())), (a, Some(18)));
    f.assert_settled();
}

/// `mv a.txt b.txt` over a file that is there: that item is here now, and the one the row
/// had here is removed, as a rename through the mount writes it.
#[test]
fn a_file_moved_over_another_items_place_has_it_and_the_other_is_removed_here() {
    let mut f = folder_of_each();
    let (a, top) = (f.id("docs/a.txt"), f.id("top.txt"));
    let b = f.copy("docs/b.txt", b"bbb", "B");
    f.rename("docs/a.txt", "docs/b.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::MovedOverAnotherItem, 1), (DiskFinding::Removed, 1)]);
    assert_eq!(f.id("docs/b.txt"), a);
    let replaced = f.item(b);
    assert_eq!((replaced.local, replaced.onedrive_item, replaced.content_presence), (None, Some(reference("d1", "B")), ContentPresence::NotOnDisk));

    // Over an item OneDrive never had: its row goes.
    f.rename("docs/b.txt", "top.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::MovedOverAnotherItem, 1), (DiskFinding::Removed, 1)]);
    assert_eq!((f.id("top.txt"), f.store.item_store().find_item(top).unwrap()), (a, None));
    f.assert_settled();
}

/// A file that is not downloaded has nothing in the backing directory to be seen: an item
/// moved to its name has the name, and the file that was not seen is not removed. It loses
/// its place, and is OneDrive's alone.
#[test]
fn a_file_moved_to_the_name_of_one_that_is_not_downloaded_has_the_name_and_removes_nothing() {
    let mut f = folder_of_each();
    let (a, remote) = (f.id("docs/a.txt"), f.id("docs/remote.bin"));
    f.rename("docs/a.txt", "docs/remote.bin");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::MovedOverAnotherItem, 1), (DiskFinding::PlaceLost, 1)]);
    assert_eq!((f.id("docs/remote.bin"), f.item(remote).local, f.item(remote).synced_place), (a, None, None));
    assert_eq!(f.item(remote).onedrive_item, Some(reference("d1", "R")));
    f.assert_settled();
}

/// Neither is where its row says: the first by its path is the item, moved, and the other a
/// copy.
#[test]
fn of_two_files_with_one_mark_one_is_the_item_and_the_other_a_new_one() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.rename("docs/a.txt", "one.txt");
    f.write("mine/two.txt", b"aaa");
    f.mark_copy("mine/two.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::MadeWithStaleMark, 1), (DiskFinding::Moved, 1)]);
    assert_eq!(f.id("one.txt"), a, "the nearer to the root, then the first by name");
    assert_eq!((f.row("mine/two.txt").unwrap().onedrive_item, f.mark_of("mine/two.txt")), (None, None));
    f.assert_settled();
}

/// A directory copied with its marks, the original where its row says: every entry of the
/// copy is a new item, whichever of the two the walk comes to first.
#[test]
fn a_directory_copied_by_hand_with_all_in_it_is_new_items_and_the_original_stays() {
    let mut f = folder_of_each();
    let (docs, a) = (f.id("docs"), f.id("docs/a.txt"));
    f.make_directory("a first copy");
    f.mark_directory("a first copy", "D");
    f.write("a first copy/a.txt", b"aaa");
    f.mark_copy("a first copy/a.txt", "A");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::MadeWithStaleMark, 2)]);
    assert_eq!((f.id("docs"), f.id("docs/a.txt")), (docs, a));
    assert_eq!(f.row("a first copy/a.txt").unwrap().onedrive_item, None);
    f.assert_settled();
}

#[test]
fn two_files_exchanged_by_hand_have_each_the_others_place() {
    let mut f = folder_of_each();
    let (a, b) = (f.id("docs/a.txt"), f.copy("b.tmp", b"bbbb", "B"));
    f.rename("docs/a.txt", "held");
    f.rename("b.tmp", "docs/a.txt");
    f.rename("held", "b.tmp");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 2)]);
    assert_eq!((f.id("docs/a.txt"), f.id("b.tmp")), (b, a));
    assert_eq!((f.item(a).is_name_ignored, f.item(b).is_name_ignored), (true, false), "each with what the rules say of its name");
    f.assert_settled();
}

/// A file at a directory's name and the directory at the file's: each is found by its mark,
/// and what is in the directory goes with it.
#[test]
fn a_file_and_a_directory_exchanged_by_hand_have_each_the_others_place() {
    let mut f = folder_of_each();
    let (docs, a, top) = (f.id("docs"), f.id("docs/a.txt"), f.copy("file", b"fff", "F"));
    f.rename("docs", "held");
    f.rename("file", "docs");
    f.rename("held", "file");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Moved, 2)]);
    assert_eq!((f.id("file"), f.id("docs"), f.id("file/a.txt")), (docs, top, a));
    assert_eq!(f.item(top).kind, ItemKind::File);
    f.assert_settled();
}

/// Three names turned round: no two of the moves can be written one after the other.
#[test]
fn three_files_whose_names_were_turned_round_by_hand_have_each_its_place() {
    let mut f = folder();
    let (a, b, c) = (f.copy("a", b"a", "A"), f.copy("b", b"bb", "B"), f.copy("c", b"ccc", "C"));
    f.rename("a", "held");
    f.rename("c", "a");
    f.rename("b", "c");
    f.rename("held", "b");
    assert_eq!(found(&f.check()), [(DiskFinding::Moved, 3)]);
    assert_eq!((f.id("a"), f.id("b"), f.id("c")), (c, a, b));
    assert_eq!(f.rows().len(), 4, "no row is left at a name of the check's own");
    f.assert_settled();
}

/// `rm -r docs; echo > docs`: the file is a new item at the directory's name. A directory of
/// the user's own is removed here with what was in it. A directory of OneDrive is not, nor
/// what OneDrive has in it: `rm -r` of it alone removes nothing in OneDrive, and a file at
/// its name does not change that. They lose their place.
#[test]
fn a_directory_replaced_by_a_file_of_its_name_goes_and_what_onedrive_has_of_it_is_not_removed() {
    let mut f = folder_of_each();
    let (docs, a, remote, mine) = (f.id("docs"), f.id("docs/a.txt"), f.id("docs/remote.bin"), f.id("mine"));
    fs::remove_dir_all(f.disk("docs")).unwrap();
    f.write("docs", b"a file now");
    fs::remove_dir_all(f.disk("mine")).unwrap();
    f.write("mine", b"m");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 1), (DiskFinding::Made, 2), (DiskFinding::Removed, 2), (DiskFinding::PlaceLost, 3)]);
    let file = f.row("docs").unwrap();
    assert_eq!((file.kind, file.onedrive_item.clone(), content(&file)), (ItemKind::File, None, changed_here(10, 1)));
    for item in [docs, a, remote] {
        let row = f.item(item);
        assert_eq!((row.local.clone(), row.synced_place.clone(), row.unsent_since), (None, None, None), "not shown, and nothing to be sent: {row:?}");
        assert!(row.onedrive_item.is_some());
    }
    assert_eq!(f.store.item_store().find_item(mine).unwrap(), None);
    f.assert_settled();
}

#[test]
fn a_file_replaced_by_a_directory_of_its_name_is_removed_and_the_directory_is_new() {
    let mut f = folder_of_each();
    let (a, top) = (f.id("docs/a.txt"), f.id("top.txt"));
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    f.make_directory("docs/a.txt");
    f.write("docs/a.txt/inside", b"i");
    fs::remove_file(f.disk("top.txt")).unwrap();
    f.make_directory("top.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::Made, 3), (DiskFinding::Removed, 2)]);
    assert_eq!((f.item(a).local, f.store.item_store().find_item(top).unwrap()), (None, None));
    assert_eq!(f.row("docs/a.txt").unwrap().kind, ItemKind::Directory);
    f.assert_settled();
}

/// A file taken out of a directory to the directory's own name, the directory removed: the
/// file cannot have the name before the directory's row is gone, and the directory's row
/// cannot go while the file's is in it.
#[test]
fn a_file_moved_to_the_name_of_the_directory_it_was_in_takes_it() {
    let mut f = folder();
    let directory = f.own_directory("box");
    f.own_file("box/other.txt", b"o");
    let a = f.copy("a.txt", b"aaa", "A");
    f.move_through_the_mount("a.txt", "box/a.txt");
    f.rename("box/a.txt", "held");
    fs::remove_dir_all(f.disk("box")).unwrap();
    f.rename("held", "box");
    assert_eq!(found(&f.check()), [(DiskFinding::MovedOverAnotherItem, 1), (DiskFinding::Removed, 2)]);
    assert_eq!((f.id("box"), f.store.item_store().find_item(directory).unwrap()), (a, None));
    assert_eq!(f.rows().len(), 2);
    f.assert_settled();
}

/// A daemon that died between marking an uploaded file and its row: the file is the row's,
/// which has no binding yet.
#[test]
fn an_item_with_no_binding_whose_entry_is_marked_with_an_id_no_row_has_is_bound_to_it() {
    let mut f = folder_of_each();
    let top = f.id("top.txt");
    f.mark_copy("top.txt", "T");
    f.mark_directory("mine", "M");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::MarkedWithoutRow, 2)]);
    let row = f.item(top);
    assert_eq!((row.onedrive_item.clone(), content(&row), row.local_item_id), (Some(reference("d1", "T")), changed_here(3, 1), top));
    assert_eq!(f.row("mine").unwrap().onedrive_item, Some(reference("d1", "M")));
    f.assert_settled();
}

/// The same, and the file edited since by hand.
#[test]
fn an_item_bound_by_its_mark_and_edited_has_its_content_changed_too() {
    let mut f = folder_of_each();
    let top = f.id("top.txt");
    f.write("top.txt", b"edited");
    f.mark_copy("top.txt", "T");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::MarkedWithoutRow, 1), (DiskFinding::ContentChanged, 1)]);
    let row = f.item(top);
    assert_eq!((row.onedrive_item.clone(), content(&row)), (Some(reference("d1", "T")), changed_here(6, 2)));
    f.assert_settled();
}

/// A writer was open when the daemon died and wrote nothing: the row is as a close that
/// changed nothing leaves it.
#[test]
fn a_file_a_dead_daemon_left_open_for_writing_and_unchanged_is_as_before_the_open() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    let before = f.item(a);
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
    assert!(f.item(a).content_before_write.is_some());
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(f.item(a), before);
}

/// Found by the test of convergence. A file and a directory of the user's own exchanged by
/// hand, an item of OneDrive in the directory: the directory at the file's name is a new
/// one and cannot be made while the file's row has the name; the file cannot have the
/// directory's name before the directory's row is gone; and that row cannot go while the
/// item in it has no directory to be moved into.
#[test]
fn a_file_exchanged_with_a_directory_that_holds_an_item_of_onedrive_has_each_at_its_place() {
    let mut f = folder();
    let file = f.copy("file", b"fff", "F");
    let directory = f.own_directory("box");
    let inside = f.copy("inside.txt", b"iii", "I");
    f.move_through_the_mount("inside.txt", "box/inside.txt");
    f.rename("file", "held");
    f.rename("box", "file");
    f.rename("held", "box");
    assert_eq!(found(&f.check()), [(DiskFinding::Made, 1), (DiskFinding::Moved, 1), (DiskFinding::MovedOverAnotherItem, 1), (DiskFinding::Removed, 1)]);
    assert_eq!((f.id("box"), f.id("file/inside.txt")), (file, inside));
    assert_eq!(f.store.item_store().find_item(directory).unwrap(), None);
    assert_eq!(f.rows().len(), 4, "no row is left at a name of the check's own");
    f.assert_settled();
}

/// Found by the test of convergence. A directory copied with its marks, the original moved
/// into the copy, and one of its files copied once more: an entry is one item, and an item
/// one entry, whichever of them the walk takes for the original.
#[test]
fn of_three_entries_with_one_mark_one_is_the_item_and_no_item_is_two_entries() {
    let mut f = folder_of_each();
    let (deep, a) = (f.onedrive_directory("docs/deep", "DD"), f.copy("docs/deep/a.txt", b"deep", "DA"));
    // `cp -a docs/deep docs/copy; mv docs/deep docs/copy/deep; cp -a docs/copy/deep/a.txt far.txt`
    f.make_directory("docs/copy");
    f.mark_directory("docs/copy", "DD");
    f.write("docs/copy/a.txt", b"deep");
    f.mark_copy("docs/copy/a.txt", "DA");
    f.rename("docs/deep", "docs/copy/deep");
    f.write("far.txt", b"deep");
    f.mark_copy("far.txt", "DA");
    let findings = f.check();
    assert_eq!(findings.unresolved(), []);
    assert_eq!((f.id("docs/copy"), f.id("docs/copy/a.txt")), (deep, a), "the first the walk comes to, and what is where its row says in it");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 7), (DiskFinding::MadeWithStaleMark, 3), (DiskFinding::Moved, 1)]);
    f.assert_settled();
}
