//! Design §10's table over a backing directory and a store: a test for each row of it, by
//! the kind of the item, its binding, and whether its content is on disk, changed here or
//! clean.

use std::fs;

use konedrive_tree::{ContentPresence, ContentVersion, FileSyncState, ItemKind, LocalVersionChange};

use super::{assert_no_item_of_onedrive_is_removed_or_moved, changed_here, content, folder, folder_of_each, found, reference, set_mtime, DiskFinding, MTIME, ROOT};

// Row 1: a file or directory, and an entry of that kind.

#[test]
fn a_file_whose_size_or_time_is_not_the_rows_has_its_content_changed_here() {
    let mut f = folder_of_each();
    f.write("docs/a.txt", b"longer than it was");
    f.write("top.txt", b"top");
    set_mtime(&f.disk("top.txt"), MTIME + 5);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::ContentChanged, 2)]);

    let a = f.row("docs/a.txt").unwrap();
    assert_eq!(content(&a), changed_here(18, 1), "changed here, with the size and the time of the disk");
    assert_eq!((a.content_presence, a.content_before_write.clone(), a.file_sync_state()), (ContentPresence::OnDisk, None, Some(FileSyncState::ModifiedHere)));
    assert!(a.onedrive_item.is_some() && a.unsent_since.is_some(), "the new content is to be sent");
    assert_eq!(f.row("top.txt").unwrap().local.unwrap().content.unwrap().mtime().0, MTIME + 5);
    f.assert_settled();
}

#[test]
fn a_clean_copy_marked_with_another_ctag_than_its_rows_is_changed_and_one_with_no_ctag_mark_is_told_by_size_and_time() {
    let mut f = folder_of_each();
    // An older copy of the same size and time, put back from a backup with its marks.
    xattr::set(f.disk("docs/a.txt"), konedrive_fs::placeholder::XATTR_CTAG, b"c-older").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    f.assert_settled();

    // A copy that landed before copies carried a cTag.
    let mut f = folder_of_each();
    xattr::remove(f.disk("docs/a.txt"), konedrive_fs::placeholder::XATTR_CTAG).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
}

/// Design §10, the first row's last sentence (the review's M1): a row that still keeps
/// the content of before a write is never made clean by size and time.
#[test]
fn a_file_a_dead_daemon_had_open_for_writing_is_changed_here_whatever_its_size_and_time_say() {
    // The size and the time of the clean copy: bytes may have been written all the same.
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    let clean = f.item(a);
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    let row = f.item(a);
    assert_eq!((content(&row), row.content_before_write.clone()), (changed_here(clean.local.as_ref().unwrap().content.as_ref().unwrap().size(), 1), None));
    assert_eq!(row.synced_content, clean.synced_content);
    f.assert_settled();

    // Bytes of another size: the size and the time of the disk, changed here.
    let mut f = folder_of_each();
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
    f.write("docs/a.txt", b"written by the program");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);
    let row = f.item(a);
    assert_eq!((content(&row), row.content_before_write.clone()), (changed_here(22, 1), None));
    f.assert_settled();
}

#[test]
fn a_directory_at_a_directorys_row_is_that_item_and_what_is_in_it_is_gone_through() {
    let mut f = folder_of_each();
    f.write("docs/new.txt", b"new!");
    f.make_directory("mine/deeper");
    f.write("mine/deeper/in.txt", b"x");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 3)]);
    f.assert_settled();
}

// Row 2: a file whose content is not on disk, and a file marked as the whole copy of that item.

#[test]
fn the_whole_copy_of_a_file_whose_row_says_not_on_disk_landed_before_its_row() {
    let mut f = folder_of_each();
    f.write("docs/remote.bin", b"123456789");
    f.mark_copy("docs/remote.bin", "R");
    let before = f.row("docs/remote.bin").unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::CopyLandedBeforeRow, 1)]);
    let row = f.row("docs/remote.bin").unwrap();
    assert_eq!((row.content_presence, row.file_sync_state()), (ContentPresence::OnDisk, Some(FileSyncState::Synced)));
    assert_eq!((content(&row), row.unsent_since), (content(&before), None), "nothing is to be sent");
    f.assert_settled();
}

// Row 3: a file whose content is not on disk, and any other file.

#[test]
fn any_other_file_where_a_rows_content_is_not_on_disk_is_content_changed_here() {
    for put in ["unmarked", "id alone", "whole copy of an older content", "another item's copy"] {
        let mut f = folder_of_each();
        match put {
            "unmarked" => f.write("docs/remote.bin", b"put there"),
            "id alone" => {
                f.write("docs/remote.bin", b"123456789");
                xattr::set(f.disk("docs/remote.bin"), konedrive_fs::placeholder::XATTR_ITEM_ID, b"R").unwrap();
            }
            "whole copy of an older content" => {
                f.write("docs/remote.bin", b"older");
                f.mark_copy("docs/remote.bin", "R");
            }
            _ => f.copy_with_marks("docs/a.txt", "docs/remote.bin"),
        }
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)], "{put}");
        let row = f.row("docs/remote.bin").unwrap();
        assert_eq!((row.content_presence, row.file_sync_state()), (ContentPresence::OnDisk, Some(FileSyncState::ModifiedHere)), "{put}");
        assert!(matches!(content(&row), Some(ContentVersion::ChangedHere { .. })), "{put}");
        assert_eq!(f.mark_of("docs/remote.bin").as_deref(), Some("R"), "{put}: the own entry of the item carries its id");
        f.assert_settled();
    }
}

// Row 4: an item OneDrive has, and nothing or an entry of the other kind.

#[test]
fn a_copy_of_onedrive_that_is_gone_is_not_shown_any_more_clean_or_changed_here_and_nothing_is_removed() {
    for was in ["clean", "changed here"] {
        let mut f = folder_of_each();
        if was == "changed here" {
            f.write("docs/a.txt", b"changed and never sent");
            f.check();
        }
        let before = f.rows();
        let a = f.id("docs/a.txt");
        fs::remove_file(f.disk("docs/a.txt")).unwrap();
        assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::NotShownAnyMore, 1)], "{was}");
        let row = f.item(a);
        assert_eq!((row.local.clone(), row.synced_place.clone(), row.synced_content.clone(), row.unsent_since), (None, None, None, None), "{was}: nothing is to be sent");
        assert_eq!(row.onedrive_item, Some(reference("d1", "A")), "{was}: the binding stays");
        assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
        f.assert_settled();
    }
}

#[test]
fn a_file_that_is_not_downloaded_and_a_directory_of_onedrive_with_nothing_on_disk_stay_as_they_are() {
    let mut f = folder();
    // As a first listing shows them: nothing of either is on disk.
    let notes = f.make_row("Notes", ItemKind::Directory, None);
    f.agree(notes, "N");
    f.not_downloaded("Notes/r.bin", 9, "R");
    let before = f.rows();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2)]);
    assert_eq!(f.rows(), before);
}

#[test]
fn a_directory_of_onedrive_that_is_gone_stays_and_its_copies_are_not_shown_any_more() {
    let mut f = folder_of_each();
    let before = f.rows();
    fs::remove_dir_all(f.disk("docs")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::NotShownAnyMore, 1)]);
    assert!(f.row("docs").is_some() && f.row("docs/remote.bin").is_some() && f.row("docs/a.txt").is_none());
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_settled();
}

#[test]
fn an_entry_of_the_other_kind_at_the_name_of_an_item_of_onedrive_is_a_new_item_and_the_item_is_not_shown_any_more() {
    let mut f = folder_of_each();
    let before = f.rows();
    let (docs, a, remote) = (f.id("docs"), f.id("docs/a.txt"), f.id("docs/remote.bin"));
    fs::remove_dir_all(f.disk("docs")).unwrap();
    f.write("docs", b"a file where the directory was");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3), (DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 3)]);
    for item in [docs, a, remote] {
        assert_eq!(f.item(item).local, None);
    }
    let made = f.row("docs").unwrap();
    assert_eq!((made.kind, made.onedrive_item.clone()), (ItemKind::File, None));
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_settled();

    // A directory at the name of a file: one whose copy was on disk, and one that was not
    // downloaded.
    let mut f = folder_of_each();
    let before = f.rows();
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    f.make_directory("docs/a.txt");
    f.make_directory("docs/remote.bin");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::Made, 2), (DiskFinding::NotShownAnyMore, 2)]);
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_settled();
}

// Row 5: an item OneDrive never had, and nothing or an entry of the other kind.

#[test]
fn an_item_onedrive_never_had_that_is_gone_goes_with_what_was_in_it() {
    let mut f = folder_of_each();
    let (mine, own, top) = (f.id("mine"), f.id("mine/own.txt"), f.id("top.txt"));
    fs::remove_dir_all(f.disk("mine")).unwrap();
    fs::remove_file(f.disk("top.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3), (DiskFinding::Removed, 3)]);
    for item in [mine, own, top] {
        assert_eq!(f.store.item_store().find_item(item).unwrap(), None);
    }
    f.assert_settled();
}

#[test]
fn an_entry_of_the_other_kind_at_the_name_of_an_item_onedrive_never_had_is_a_new_item() {
    let mut f = folder_of_each();
    let (mine, top) = (f.id("mine"), f.id("top.txt"));
    fs::remove_dir_all(f.disk("mine")).unwrap();
    f.write("mine", b"a file");
    fs::remove_file(f.disk("top.txt")).unwrap();
    f.make_directory("top.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3), (DiskFinding::Made, 2), (DiskFinding::Removed, 3)]);
    assert_ne!(f.id("mine"), mine);
    assert_ne!(f.id("top.txt"), top);
    assert_eq!((f.row("mine").unwrap().kind, f.row("top.txt").unwrap().kind), (ItemKind::File, ItemKind::Directory));
    f.assert_settled();
}

/// The item of OneDrive in a directory OneDrive does not have leaves the folder with the
/// directory's row: it is not removed.
#[test]
fn what_onedrive_has_below_a_row_that_goes_is_not_shown_any_more() {
    let mut f = folder_of_each();
    f.move_through_the_mount("docs/remote.bin", "mine/remote.bin");
    f.move_through_the_mount("docs/a.txt", "mine/a.txt");
    let before = f.rows();
    let (a, remote) = (f.id("mine/a.txt"), f.id("mine/remote.bin"));
    fs::remove_dir_all(f.disk("mine")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 2), (DiskFinding::NotShownAnyMore, 2), (DiskFinding::Removed, 2)]);
    for item in [a, remote] {
        let row = f.item(item);
        assert_eq!((row.local.clone(), row.synced_place.clone(), row.unsent_since), (None, None, None));
        assert!(row.onedrive_item.is_some());
    }
    assert_eq!(super::bindings(&before), super::bindings(&f.rows()));
    f.assert_settled();
}

// Row 7: nothing, and any other entry. (Row 6 is the rebuilt store's: `rebuilt_store`.)

#[test]
fn a_file_and_a_directory_made_by_hand_are_new_items_with_what_the_rules_say_of_their_names() {
    let mut f = folder_of_each();
    f.write("docs/new.txt", b"new!");
    f.make_directory("cache");
    f.write("cache/in.txt", b"x");
    f.write("a.tmp", b"t");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 4)]);

    let new = f.row("docs/new.txt").unwrap();
    assert_eq!((new.kind, new.onedrive_item.clone(), content(&new)), (ItemKind::File, None, changed_here(4, 1)));
    assert_eq!((new.content_presence, new.is_name_ignored, new.synced_content.clone()), (ContentPresence::OnDisk, false, None));
    assert_eq!(new.local.unwrap().place.unwrap().parent_local_item_id, f.id("docs"));
    let cache = f.row("cache").unwrap();
    assert_eq!((cache.kind, cache.is_name_ignored, cache.local.unwrap().place.unwrap().parent_local_item_id), (ItemKind::Directory, true, ROOT));
    let inside = f.row("cache/in.txt").unwrap();
    assert!(!inside.is_name_ignored && f.store.item_store().is_ignored(inside.local_item_id).unwrap(), "left here with its directory");
    assert!(f.row("a.tmp").unwrap().is_name_ignored);
    f.assert_settled();
}

#[test]
fn an_entry_that_carries_the_mark_of_an_item_that_has_a_row_is_a_new_item_and_the_mark_is_taken_off() {
    let mut f = folder_of_each();
    let before = f.rows();
    f.copy_with_marks("docs/a.txt", "a-copy.txt");
    f.make_directory("docs-copy");
    f.mark_directory("docs-copy", "D");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 2)]);
    for made in ["a-copy.txt", "docs-copy"] {
        assert_eq!(f.row(made).unwrap().onedrive_item, None, "{made}");
        assert_eq!(f.mark_of(made), None, "{made}");
    }
    assert_eq!(f.copy_marks_of("a-copy.txt"), (false, None), "the state and the cTag go with the id");
    assert_eq!((f.mark_of("docs/a.txt").as_deref(), f.copy_marks_of("docs/a.txt")), (Some("A"), (true, Some("c-A".to_owned()))), "the item's own entry keeps its marks");
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_settled();
}

/// What the design says of a move made by hand: a new file at its new place, and the item
/// it was shown again at its old one. Neither is a move in OneDrive.
#[test]
fn a_file_moved_by_hand_is_a_new_file_and_its_item_is_not_shown_any_more() {
    let mut f = folder_of_each();
    let before = f.rows();
    let a = f.id("docs/a.txt");
    f.rename("docs/a.txt", "moved.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 1), (DiskFinding::NotShownAnyMore, 1)]);
    assert_ne!(f.id("moved.txt"), a);
    assert_eq!((f.row("moved.txt").unwrap().onedrive_item, f.mark_of("moved.txt")), (None, None));
    assert_no_item_of_onedrive_is_removed_or_moved(&before, &f.rows());
    f.assert_settled();
}
