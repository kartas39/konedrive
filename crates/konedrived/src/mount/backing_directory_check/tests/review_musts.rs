//! The MUSTs of the three reviews of the check that followed moves, copies and removals by
//! the marks (`docs/tasks/item-versions-part-5-review.txt`, `-review-2.txt`,
//! `-review-3.txt`), each as its sequence and the outcome it has now. In every one of them
//! nothing is removed or moved in the rows of an item OneDrive has: such an item is at
//! most not shown any more, with nothing to be sent.

use std::fs;
use std::path::PathBuf;

use konedrive_tree::{ContentPresence, ItemKind};

use super::unread_parts::{check_with_unreadable, is_root};
use super::{assert_no_item_of_onedrive_is_removed_or_moved, bindings, changed_here, content, folder, folder_of_each, found, left, reference, DiskFinding, Folder};

/// The check, with the assertion every sequence here ends in.
fn check_removing_and_moving_nothing_of_onedrive(f: &mut Folder) -> Vec<(DiskFinding, usize)> {
    let before = f.rows();
    let findings = f.check();
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_plan_sends_no_removal_or_move("after the check");
    found(&findings)
}

// ---- Review 1 ----

/// M1 a): `mv top.txt secret/ ; chmod 000 secret`. The row of the user's own file goes; the
/// file is a new item at the next start that can read.
#[test]
fn review_1_m1_a_a_file_of_the_users_own_moved_into_a_directory_that_cannot_be_read() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.make_directory("secret");
    f.rename("top.txt", "secret/top.txt");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::Removed, 1)]);
    assert_eq!(left(&findings), [PathBuf::from("secret")]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 2)]);
    f.assert_settled();
}

/// M1 b): an edited, unsent file of OneDrive and a directory with a clean copy in it, moved
/// into a directory that cannot be read. Two removals were to be sent; none is.
#[test]
fn review_1_m1_b_items_of_onedrive_moved_into_a_directory_that_cannot_be_read() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.copy("docs/b.txt", b"bbb", "B");
    f.write("docs/b.txt", b"edited through the mount, unsent");
    f.check();
    f.own_directory("box");
    f.copy("docs/c.txt", b"ccc", "C");
    f.move_through_the_mount("docs/c.txt", "box/c.txt");
    let (b, c) = (f.id("docs/b.txt"), f.id("box/c.txt"));
    f.make_directory("secret");
    f.rename("docs/b.txt", "secret/b.txt");
    f.rename("box", "secret/box");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "secret");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 6), (DiskFinding::NotShownAnyMore, 2), (DiskFinding::Removed, 1)]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    for item in [b, c] {
        let row = f.item(item);
        assert_eq!((row.local.clone(), row.synced_place.clone(), row.unsent_since), (None, None, None), "no removal is to be sent");
    }
    assert_eq!(bindings(&before), bindings(&f.rows()));
    // Readable: what is in it are new files of the user's own, the edit among them.
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 4)]);
    assert_eq!(content(&f.row("secret/b.txt").unwrap()), changed_here(32, 1));
    f.assert_settled();
}

/// M1 c): `mv docs docs2 ; chmod 000 docs2`.
#[test]
fn review_1_m1_c_a_directory_of_onedrive_renamed_and_made_unreadable() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.rename("docs", "docs2");
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "docs2");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::NotShownAnyMore, 1)]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert!(f.row("docs").is_some() && f.row("docs2").is_none());
}

/// M2 a): a file of OneDrive that is not downloaded, in a directory of the user's own that
/// is renamed by hand.
#[test]
fn review_1_m2_a_a_directory_of_the_users_own_renamed_with_a_file_of_onedrive_not_on_disk_in_it() {
    let mut f = folder_of_each();
    f.own_directory("new");
    f.move_through_the_mount("docs/remote.bin", "new/remote.bin");
    let remote = f.id("new/remote.bin");
    f.rename("new", "new2");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 1), (DiskFinding::Removed, 1)]);
    let row = f.item(remote);
    assert_eq!((row.local.clone(), row.onedrive_item.clone(), row.unsent_since), (None, Some(reference("d1", "R")), None));
    f.assert_settled();
}

/// M2 b): a file saved at the name of a directory of OneDrive that has nothing on disk: the
/// whole tree was removed in OneDrive.
#[test]
fn review_1_m2_b_a_file_at_the_name_of_a_directory_of_onedrive_that_has_none_on_disk() {
    let mut f = folder();
    let notes = f.make_row("Notes", ItemKind::Directory, None);
    f.agree(notes, "N");
    let sub = f.make_row("Notes/sub", ItemKind::Directory, None);
    f.agree(sub, "NS");
    let (r1, r2) = (f.not_downloaded("Notes/r1.bin", 9, "R1"), f.not_downloaded("Notes/sub/r2.bin", 9, "R2"));
    let before = f.rows();
    f.write("Notes", b"a note");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 4)]);
    for item in [notes, sub, r1, r2] {
        assert_eq!((f.item(item).local, f.item(item).unsent_since), (None, None));
    }
    assert_eq!(bindings(&before), bindings(&f.rows()));
    assert_eq!(f.row("Notes").unwrap().kind, ItemKind::File);
    f.assert_settled();
}

/// M2 c): `mkdir report` where `report` is a file that is not downloaded.
#[test]
fn review_1_m2_c_a_directory_made_at_the_name_of_a_file_that_is_not_downloaded() {
    let mut f = folder();
    let report = f.not_downloaded("report", 9, "R");
    f.make_directory("report");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((f.item(report).local, f.item(report).onedrive_item), (None, Some(reference("d1", "R"))));
    f.assert_settled();
}

/// M3 a) and b): one item id in two drives, nothing touched by hand.
#[test]
fn review_1_m3_one_item_id_in_two_drives_with_nothing_touched() {
    for is_first_on_disk in [false, true] {
        let mut f = folder();
        f.directory_of_drive("shared", "d2", "S");
        f.file_of_drive("l.bin", "d1", "X", is_first_on_disk);
        f.file_of_drive("shared/h.txt", "d2", "X", true);
        let before = f.rows();
        assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 3)]);
        assert_eq!(f.rows(), before);
        assert_eq!((f.mark_of("shared/h.txt").as_deref(), f.copy_marks_of("shared/h.txt")), (Some("X"), (true, Some("c-X".to_owned()))));
    }
}

/// M4: a copy bound by its mark in a rebuilt store, gone before a listing agreed on it.
#[test]
fn review_1_m4_a_copy_bound_by_its_mark_that_is_gone_before_the_plan_compared_it() {
    let mut f = folder();
    f.write("lost.txt", b"lost");
    f.mark_copy("lost.txt", "Z");
    assert_eq!(found(&f.check()), [(DiskFinding::MarkedWithoutRow, 1)]);
    let lost = f.id("lost.txt");
    fs::remove_file(f.disk("lost.txt")).unwrap();
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NotShownAnyMore, 1)]);
    let row = f.item(lost);
    assert_eq!((row.local.clone(), row.onedrive_item.clone(), row.unsent_since), (None, Some(reference("d1", "Z")), None));
}

// ---- Review 2 ----

/// M1: one item id in two drives; one twin is saved by an editor that renames a new file
/// over it. Its removal was sent.
#[test]
fn review_2_m1_a_twin_saved_by_rename() {
    for marks_alone_taken_off in [false, true] {
        let mut f = folder();
        f.directory_of_drive("shared", "d2", "S");
        let l = f.file_of_drive("l.bin", "d1", "X", true);
        f.file_of_drive("shared/h.txt", "d2", "X", true);
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3)]);
        let expected = if marks_alone_taken_off {
            for mark in [konedrive_fs::placeholder::XATTR_ITEM_ID, konedrive_fs::placeholder::XATTR_STATE, konedrive_fs::placeholder::XATTR_CTAG] {
                xattr::remove(f.disk("l.bin"), mark).unwrap();
            }
            vec![(DiskFinding::NothingChanged, 3)]
        } else {
            f.write("l.bin", b"saved by rename");
            vec![(DiskFinding::NothingChanged, 2), (DiskFinding::ContentChanged, 1)]
        };
        assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), expected);
        assert_eq!((f.id("l.bin"), f.item(l).onedrive_item), (l, Some(reference("d1", "X"))));
        assert_eq!(f.mark_of("l.bin").as_deref(), Some("X"));
        f.assert_settled();
    }
}

/// M2: the whole copy of d1!X comes to be in the root while the folder shows no row of it;
/// d2!X, which is not downloaded, was moved to it and had its content replaced.
#[test]
fn review_2_m2_a_marked_file_is_never_taken_as_the_other_drives_item() {
    for has_unshown_row in [false, true] {
        let mut f = folder();
        f.directory_of_drive("shared", "d2", "S");
        let x = f.file_of_drive("shared/x.bin", "d2", "X", false);
        if has_unshown_row {
            f.store.item_store().make_onedrive_item(&reference("d1", "X"), ItemKind::File).unwrap();
        }
        f.write("a.txt", b"the whole copy of d1!X");
        f.mark_copy("a.txt", "X");
        let before = f.item(x);
        assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 2), (DiskFinding::Made, 1)]);
        assert_eq!(f.item(x), before, "d2!X is where it was, with nothing on disk");
        assert_eq!((f.row("a.txt").unwrap().onedrive_item, f.mark_of("a.txt")), (None, None));
        f.assert_settled();
    }
}

/// M3: `cp -a docs docs-copy` without a.txt; `cp -a docs/a.txt a-copy.txt`; `chmod 000 docs`.
/// The item, which nobody touched, was moved to the copy.
#[test]
fn review_2_m3_an_untouched_item_in_a_directory_that_cannot_be_read_and_copies_of_it() {
    if is_root() {
        return;
    }
    let mut f = folder_of_each();
    f.make_directory("docs-copy");
    f.mark_directory("docs-copy", "D");
    f.copy_with_marks("docs/a.txt", "a-copy.txt");
    let a = f.id("docs/a.txt");
    let a = f.item(a);
    let before = f.rows();
    let findings = check_with_unreadable(&mut f, "docs");
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 3), (DiskFinding::Made, 2)]);
    assert_eq!(left(&findings), [PathBuf::from("docs")]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!(f.item(a.local_item_id), a);
    // Readable again: the item is where it was, with its id and its marks.
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 8)]);
    assert_eq!((f.item(a.local_item_id), f.mark_of("docs/a.txt").as_deref(), f.copy_marks_of("docs/a.txt")), (a, Some("A"), (true, Some("c-A".to_owned()))));
    f.assert_settled();
}

// ---- Review 3 ----

/// M1: a file saved by rename gets the id of its item as its mark; trashed and restored, it
/// had no row, was not shown and was never sent.
#[test]
fn review_3_m1_a_file_the_check_marked_trashed_and_restored() {
    let mut f = folder_of_each();
    let trash = tempfile::tempdir_in(f.disk("").parent().unwrap()).unwrap();
    f.write("docs/a.txt", b"saved by rename");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.copy_marks_of("docs/a.txt")), (Some("A"), (false, None)));
    let a = f.id("docs/a.txt");

    fs::rename(f.disk("docs/a.txt"), trash.path().join("a.txt")).unwrap();
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 5), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((f.item(a).local, f.item(a).unsent_since), (None, None), "no removal is to be sent");

    fs::rename(trash.path().join("a.txt"), f.disk("docs/a.txt")).unwrap();
    let before = f.rows();
    let findings = f.check();
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!((found(&findings), findings.unresolved()), (vec![(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 1)], &[][..]));
    // Its bytes are shown, and are to be sent, as a new file of the user's own.
    let row = f.row("docs/a.txt").unwrap();
    assert_eq!((row.onedrive_item.clone(), content(&row), row.content_presence), (None, changed_here(15, 1), ContentPresence::OnDisk));
    assert!(row.unsent_since.is_some());
    assert_eq!(f.mark_of("docs/a.txt"), None);
    f.assert_settled();
}

/// M2: `ln docs/a.txt a-link.txt`, start after start, and then `mv docs/a.txt docs/b.txt`.
#[test]
fn review_3_m2_a_hard_link_and_then_a_rename() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.link("docs/a.txt", "a-link.txt");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1)]);
    for _ in 0..4 {
        f.assert_settled();
    }
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.mark_of("a-link.txt").as_deref()), (Some("A"), Some("A")), "no mark was taken off or written through either name");

    f.rename("docs/a.txt", "docs/b.txt");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((f.item(a).local, f.item(a).unsent_since, f.item(a).onedrive_item), (None, None, Some(reference("d1", "A"))));
    f.assert_settled();
}

/// M2, both in one stop: `ln docs/a.txt a-link.txt ; mv docs/a.txt docs/b.txt`. The item
/// was moved to the link.
#[test]
fn review_3_m2_a_hard_link_and_a_rename_in_one_stop() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.link("docs/a.txt", "a-link.txt");
    f.rename("docs/a.txt", "docs/b.txt");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 2), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!(f.item(a).local, None);
    f.assert_settled();
}

/// M2, two items made one file (`ln -f docs/a.txt docs/b.txt`): the mark alternated between
/// the two ids, one of them was sent again at every start, and a rename removed the other.
#[test]
fn review_3_m2_two_items_on_one_file() {
    let mut f = folder_of_each();
    let b = f.copy("docs/b.txt", b"bbb", "B");
    fs::remove_file(f.disk("docs/b.txt")).unwrap();
    f.link("docs/a.txt", "docs/b.txt");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 7)], "of the size and time of both rows; the cTag mark of a file with two names is not asked");
    for _ in 0..4 {
        f.assert_settled();
    }
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.mark_of("docs/b.txt").as_deref()), (Some("A"), Some("A")));
    // Review S4: an upload of one name marks the file as that name's item; the other
    // name's row is not "changed here" for it, at this start or the next.
    f.mark_copy("docs/a.txt", "B");
    for _ in 0..2 {
        assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 7)]);
    }
    f.mark_copy("docs/a.txt", "A");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 7)]);

    f.rename("docs/b.txt", "docs/c.txt");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((f.item(b).local, f.item(b).unsent_since, f.item(b).onedrive_item), (None, None, Some(reference("d1", "B"))));
    f.assert_settled();
}

/// M3: `cp -a p/a.txt a-copy.txt ; mv x/q q2 ; mv p q2/p`. The item was moved to the copy,
/// out of its directory.
#[test]
fn review_3_m3_an_item_in_a_moved_directory_and_a_copy_of_it_nearer_the_root() {
    let mut f = folder();
    let p = f.onedrive_directory("p", "P");
    let a = f.copy("p/a.txt", b"aaa", "A");
    f.onedrive_directory("x", "X");
    let q = f.onedrive_directory("x/q", "Q");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4)]);
    let places = |f: &mut Folder| [p, q].map(|item| f.item(item).local.unwrap().place);

    let before = places(&mut f);
    f.copy_with_marks("p/a.txt", "a-copy.txt");
    f.rename("x/q", "q2");
    f.rename("p", "q2/p");
    assert_eq!(check_removing_and_moving_nothing_of_onedrive(&mut f), [(DiskFinding::NothingChanged, 3), (DiskFinding::Made, 4), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!(places(&mut f), before, "no directory of OneDrive is moved");
    assert_eq!((f.item(a).local, f.item(a).unsent_since, f.item(a).onedrive_item), (None, None, Some(reference("d1", "A"))));
    for new in ["a-copy.txt", "q2", "q2/p", "q2/p/a.txt"] {
        assert_eq!((f.row(new).unwrap().onedrive_item, f.mark_of(new)), (None, None), "{new}");
    }
    f.assert_settled();
}

/// M4: `mv docs old ; mkdir docs ; mv old docs/old ; echo new > docs/new.txt`. Everything in
/// docs was left for good, said as "may be unread".
#[test]
fn review_3_m4_a_directory_of_onedrive_put_into_a_new_directory_of_its_own_name() {
    let mut f = folder_of_each();
    let docs = f.id("docs");
    f.rename("docs", "old");
    f.make_directory("docs");
    f.rename("old", "docs/old");
    f.write("docs/new.txt", b"new");
    let before = f.rows();
    let findings = f.check();
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    assert_eq!(found(&findings), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 3), (DiskFinding::NotShownAnyMore, 1)]);
    assert_eq!((findings.unresolved(), findings.list_held_items(), findings.has_unread_part()), (&[][..], &[][..], false), "nothing is left");
    assert_eq!((f.id("docs"), f.mark_of("docs").as_deref()), (docs, Some("D")), "the directory at the row's place is the item");
    for new in ["docs/new.txt", "docs/old", "docs/old/a.txt"] {
        assert_eq!((f.row(new).unwrap().onedrive_item, f.mark_of(new)), (None, None), "{new}");
    }
    f.assert_settled();
}
