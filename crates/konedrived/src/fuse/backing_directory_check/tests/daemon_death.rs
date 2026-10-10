//! A daemon that died between a change on disk and its row, for each operation of the
//! backing directory (`docs/tasks/item-versions-design.md`, §8): the state it leaves is
//! given to the check, and the rows afterwards are what the operation would have written.

use std::fs;

use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalVersionChange};

use super::{changed_here, content, folder_of_each, found, reference, DiskFinding, Folder, MTIME, ROOT};

/// The rows of a folder the operation was written to: what the check must arrive at.
fn written(operation: impl FnOnce(&mut Folder)) -> Vec<Item> {
    let mut f = folder_of_each();
    operation(&mut f);
    f.rows()
}

/// A file's content is changed, the row first: the first writer's open raised the count,
/// bytes went to disk, and no close was written.
#[test]
fn a_death_with_a_writer_open_leaves_what_the_close_would_have_written() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
    fs::write(f.disk("docs/a.txt"), b"written before the death").unwrap();
    super::set_mtime(&f.disk("docs/a.txt"), MTIME + 5);
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentChanged, 1)]);

    let expected = written(|f| {
        f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
        f.store.item_store().apply(&LocalVersionChange::ClosedChanged { item: a, size: 24, mtime: MTIME + 5, mtime_nsec: 0 }).unwrap();
    });
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// The same, and the writer had written nothing.
#[test]
fn a_death_with_a_writer_open_that_wrote_nothing_leaves_what_a_close_that_changed_nothing_would_have() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6)]);
    assert_eq!(f.rows(), written(|_| {}));
    f.assert_settled();
}

/// Make: the disk first, the row second.
#[test]
fn a_death_after_a_file_or_directory_was_made_leaves_its_row() {
    let mut f = folder_of_each();
    f.write("docs/made.txt", b"");
    f.make_directory("docs/made");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 6), (DiskFinding::Made, 2)]);

    let docs = f.id("docs");
    let expected = written(|f| {
        for (name, kind, content) in [("made", ItemKind::Directory, None), ("made.txt", ItemKind::File, changed_here(0, 1))] {
            let place = ItemPlace { parent_local_item_id: docs, name: name.into() };
            f.store.item_store().apply(&LocalVersionChange::Made { kind, place, content, is_name_ignored: false, replaced: None }).unwrap();
        }
    });
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// Rename: the disk first, the row second. An item of OneDrive is known by its mark.
#[test]
fn a_death_after_an_item_of_onedrive_was_renamed_leaves_its_row_at_the_new_place() {
    let mut f = folder_of_each();
    let (a, docs) = (f.id("docs/a.txt"), f.id("docs"));
    f.rename("docs/a.txt", "b.tmp");
    f.rename("docs", "papers");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 4), (DiskFinding::Moved, 2)]);

    let expected = written(|f| {
        let moved = LocalVersionChange::Moved { item: a, place: ItemPlace { parent_local_item_id: ROOT, name: "b.tmp".into() }, is_name_ignored: true };
        f.store.item_store().apply(&moved).unwrap();
        let moved = LocalVersionChange::Moved { item: docs, place: ItemPlace { parent_local_item_id: ROOT, name: "papers".into() }, is_name_ignored: false };
        f.store.item_store().apply(&moved).unwrap();
    });
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// An item OneDrive never had carries no mark: after a death its rename is read as a removal
/// and a new item. The folder shows the same; the item's number is another.
#[test]
fn a_death_after_an_item_onedrive_never_had_was_renamed_leaves_a_new_item_at_the_new_place() {
    let mut f = folder_of_each();
    let top = f.id("top.txt");
    let top = f.item(top);
    f.rename("top.txt", "renamed.txt");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::Made, 1), (DiskFinding::Removed, 1)]);
    let renamed = f.row("renamed.txt").unwrap();
    assert_ne!(renamed.local_item_id, top.local_item_id);
    let mut expected = top.clone();
    expected.local_item_id = renamed.local_item_id;
    expected.local.as_mut().unwrap().place.as_mut().unwrap().name = "renamed.txt".into();
    assert_eq!(renamed, expected);
    f.assert_settled();
}

/// A rename over another item: the disk first, the row second.
#[test]
fn a_death_after_an_item_was_renamed_over_another_leaves_what_the_rename_would_have_written() {
    let mut f = folder_of_each();
    let (a, top) = (f.id("docs/a.txt"), f.id("top.txt"));
    let b = f.copy("docs/b.txt", b"bbb", "B");
    f.rename("docs/a.txt", "docs/b.txt");
    f.check();
    f.rename("docs/b.txt", "top.txt");
    f.check();

    let expected = written(|f| {
        f.copy("docs/b.txt", b"bbb", "B");
        f.store.item_store().apply(&LocalVersionChange::SavedOver { replacing: a, replaced: b }).unwrap();
        // A file of OneDrive takes no binding over: the other is removed, as today's rename records it.
        f.store.item_store().apply(&LocalVersionChange::Removed { item: top }).unwrap();
        let place = ItemPlace { parent_local_item_id: ROOT, name: "top.txt".into() };
        f.store.item_store().apply(&LocalVersionChange::Moved { item: a, place, is_name_ignored: false }).unwrap();
    });
    assert_eq!(f.rows(), expected);
}

/// Remove: the disk first, the row second.
#[test]
fn a_death_after_an_item_was_removed_leaves_it_removed_here() {
    let mut f = folder_of_each();
    let (top, own, mine) = (f.id("top.txt"), f.id("mine/own.txt"), f.id("mine"));
    fs::remove_file(f.disk("top.txt")).unwrap();
    fs::remove_dir_all(f.disk("mine")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 3), (DiskFinding::Removed, 3)]);

    let expected = written(|f| {
        for item in [top, own, mine] {
            f.store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
        }
    });
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// The removal of a clean downloaded copy that died before its row is the one the check
/// does not finish: a power loss leaves the same picture, so the file is not on disk and
/// nothing is removed (the design's §13, 2).
#[test]
fn a_death_after_a_clean_copy_was_removed_leaves_the_file_not_downloaded() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    fs::remove_file(f.disk("docs/a.txt")).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::ContentNoLongerOnDisk, 1)]);
    let expected = written(|f| f.store.item_store().set_content_presence(a, ContentPresence::NotOnDisk).unwrap());
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// A download lands: the file is synced and given its name, and then the row says on disk.
#[test]
fn a_death_after_a_download_landed_leaves_its_content_on_disk() {
    let mut f = folder_of_each();
    let remote = f.id("docs/remote.bin");
    f.write("docs/remote.bin", b"123456789");
    f.mark_copy("docs/remote.bin", "R");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::CopyLandedBeforeRow, 1)]);
    let expected = written(|f| f.store.item_store().set_content_presence(remote, ContentPresence::OnDisk).unwrap());
    assert_eq!(f.rows(), expected);
    f.assert_settled();
}

/// A copy is freed: the row says not on disk first, then the file goes. After a death
/// between the two the copy is whole and still there, and the row says so again: the
/// freeing is undone, and nothing is lost.
#[test]
fn a_death_while_a_copy_was_freed_leaves_the_copy_on_disk() {
    let mut f = folder_of_each();
    let a = f.id("docs/a.txt");
    f.store.item_store().set_content_presence(a, ContentPresence::NotOnDisk).unwrap();
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::CopyLandedBeforeRow, 1)]);
    assert_eq!(f.rows(), written(|_| {}));
    f.assert_settled();
}

/// An upload's commit marks the file and then binds its row. After a death between the two
/// the row is bound to the id of the mark; what was agreed on is the listing's to bring.
#[test]
fn a_death_after_an_uploaded_file_was_marked_leaves_its_row_bound() {
    let mut f = folder_of_each();
    let top = f.id("top.txt");
    f.mark_copy("top.txt", "T");
    assert_eq!(found(&f.check()), [(DiskFinding::NothingChanged, 5), (DiskFinding::MarkedWithoutRow, 1)]);
    let expected = written(|f| f.store.item_store().bind_item(top, &reference("d1", "T")).unwrap());
    assert_eq!(f.rows(), expected);
    assert_eq!(content(&f.item(top)), Some(ContentVersion::ChangedHere { size: 3, mtime: MTIME, mtime_nsec: 0, change_count: 1 }));
}
