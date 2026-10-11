//! Rename: the two decisions' cases, and each rename over the fixture.

use std::ffi::OsStr;

use konedrive_tree::{ContentVersion, FileSyncState};

use super::*;
use crate::fuse::item_fixture::{onedrive_id_of, ItemFixture};
use crate::fuse::item_write_operations::tests::{bind, make_row, take_off_disk, OperationPoint, OperationsFixture};
use crate::fuse::item_write_operations::MadeKind;
use crate::fuse::mount_take_down::TakeDownCause;

/// The rename of `moved`, which is on disk, into a directory the folder shows, to a name
/// nothing has.
fn case(moved: Item) -> RenameCase {
    RenameCase {
        moved: Some(moved),
        moved_path: Some("a/x".into()),
        new_directory: Some(make_row(3, ItemKind::Directory)),
        new_directory_path: Some("b".into()),
        new_directory_marks: vec![None],
        displaced: None,
        is_displaced_empty: true,
        is_into_itself: false,
        mode: RenameMode::Replace,
        is_new_name_hidden: false,
        is_new_name_ignored: false,
    }
}

#[test]
fn each_case_of_a_rename_is_decided() {
    let (file, directory) = (make_row(5, ItemKind::File), make_row(6, ItemKind::Directory));
    let (other_file, other_directory) = (make_row(7, ItemKind::File), make_row(8, ItemKind::Directory));
    for moved in [&file, &directory] {
        let item = moved.local_item_id;
        assert_eq!(decide_rename(&case(moved.clone())), Ok(RenameDecision::Move { item, is_name_ignored: false }));
        assert_eq!(decide_rename(&RenameCase { is_new_name_ignored: true, ..case(moved.clone()) }), Ok(RenameDecision::Move { item, is_name_ignored: true }));
        assert_eq!(decide_rename(&RenameCase { mode: RenameMode::NoReplace, ..case(moved.clone()) }), Ok(RenameDecision::Move { item, is_name_ignored: false }), "nothing is there to keep");
        assert_eq!(decide_rename(&RenameCase { mode: RenameMode::Exchange, displaced: Some(other_file.clone()), ..case(moved.clone()) }), Err(Errno::EINVAL), "an exchange");
        assert_eq!(decide_rename(&RenameCase { new_directory: None, ..case(moved.clone()) }), Err(Errno::ENOENT));
        assert_eq!(decide_rename(&RenameCase { is_new_name_hidden: true, ..case(moved.clone()) }), Err(Errno::EPERM));
        assert_eq!(decide_rename(&RenameCase { displaced: Some(moved.clone()), ..case(moved.clone()) }), Ok(RenameDecision::Nothing), "the same place");
        assert_eq!(decide_rename(&RenameCase { displaced: Some(moved.clone()), mode: RenameMode::NoReplace, ..case(moved.clone()) }), Ok(RenameDecision::Nothing));
        // An item of OneDrive is moved as one that is not: also into a directory that is
        // left here, where today it is `EXDEV`.
        assert_eq!(decide_rename(&RenameCase { is_new_name_ignored: true, ..case(bind(moved.clone())) }), Ok(RenameDecision::Move { item, is_name_ignored: true }));
    }
    assert_eq!(decide_rename(&RenameCase { moved: None, moved_path: None, ..case(file.clone()) }), Err(Errno::ENOENT), "nothing at the old place");
    // Today's order: the new directory, the new name, then what moves.
    assert_eq!(decide_rename(&RenameCase { moved: None, is_new_name_hidden: true, ..case(file.clone()) }), Err(Errno::EPERM));
    assert_eq!(decide_rename(&RenameCase { moved: None, new_directory: None, is_new_name_hidden: true, ..case(file.clone()) }), Err(Errno::ENOENT));
    // A directory into itself or below.
    assert_eq!(decide_rename(&RenameCase { is_into_itself: true, ..case(directory.clone()) }), Err(Errno::EINVAL));
    assert_eq!(decide_rename(&RenameCase { is_into_itself: true, displaced: Some(other_directory.clone()), ..case(directory.clone()) }), Err(Errno::EINVAL));
    // Over an entry.
    let over = |moved: &Item, displaced: &Item| RenameCase { displaced: Some(displaced.clone()), ..case(moved.clone()) };
    assert_eq!(decide_rename(&over(&file, &other_file)), Ok(RenameDecision::SaveOver { replacing: file.local_item_id, replaced: other_file.local_item_id }));
    assert_eq!(decide_rename(&over(&file, &take_off_disk(bind(other_file.clone())))), Ok(RenameDecision::SaveOver { replacing: file.local_item_id, replaced: other_file.local_item_id }), "over a file that is not on disk");
    assert_eq!(decide_rename(&over(&directory, &other_directory)), Ok(RenameDecision::SaveOver { replacing: directory.local_item_id, replaced: other_directory.local_item_id }));
    assert_eq!(decide_rename(&RenameCase { is_displaced_empty: false, ..over(&directory, &other_directory) }), Err(Errno::ENOTEMPTY));
    assert_eq!(decide_rename(&over(&file, &other_directory)), Err(Errno::EISDIR));
    assert_eq!(decide_rename(&over(&directory, &other_file)), Err(Errno::ENOTDIR));
    for (moved, displaced) in [(&file, &other_file), (&file, &other_directory), (&directory, &other_file), (&directory, &other_directory)] {
        assert_eq!(decide_rename(&RenameCase { mode: RenameMode::NoReplace, ..over(moved, displaced) }), Err(Errno::EEXIST), "the mode keeps what is there");
    }
}

#[test]
fn each_case_of_what_a_rename_does_on_disk_is_decided() {
    let (file, directory) = (make_row(5, ItemKind::File), make_row(6, ItemKind::Directory));
    let (other_file, other_directory) = (make_row(7, ItemKind::File), make_row(8, ItemKind::Directory));
    let moving = RenameDecision::Move { item: file.local_item_id, is_name_ignored: false };
    let not_on_disk = |case: RenameCase| RenameCase { moved_path: None, ..case };
    assert_eq!(decide_rename_disk_change(&case(file.clone()), RenameDecision::Nothing), RenameDiskChange::Nothing);
    assert_eq!(decide_rename_disk_change(&case(file.clone()), moving), RenameDiskChange::MoveEntry { from: "a/x".into() });
    assert_eq!(decide_rename_disk_change(&not_on_disk(case(file.clone())), moving), RenameDiskChange::Nothing, "nothing of it is on disk");
    let saving = |replaced: &Item| RenameDecision::SaveOver { replacing: file.local_item_id, replaced: replaced.local_item_id };
    let over = |moved: &Item, displaced: Item| RenameCase { displaced: Some(displaced), ..case(moved.clone()) };
    // Whatever the displaced one has on disk, the place change looks: it sets aside what is there.
    for displaced in [other_file.clone(), take_off_disk(other_file.clone())] {
        assert_eq!(decide_rename_disk_change(&over(&file, displaced), saving(&other_file)), RenameDiskChange::MoveEntryOver { from: "a/x".into(), replaced: other_file.local_item_id });
    }
    assert_eq!(decide_rename_disk_change(&not_on_disk(over(&file, other_file.clone())), saving(&other_file)), RenameDiskChange::SetAside { replaced: other_file.local_item_id });
    assert_eq!(decide_rename_disk_change(&not_on_disk(over(&file, take_off_disk(other_file.clone()))), saving(&other_file)), RenameDiskChange::Nothing, "neither is on disk");
    // A displaced directory may be on disk whatever its row says.
    assert_eq!(decide_rename_disk_change(&not_on_disk(over(&directory, other_directory.clone())), saving(&other_directory)), RenameDiskChange::SetAside { replaced: other_directory.local_item_id });
}

#[test]
fn an_item_renamed_to_a_free_name_is_moved_on_disk_and_then_in_its_row() {
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("old.txt", b"mine").local_item_id;
    let docs = operations.fixture.show_onedrive_directory("docs").local_item_id;
    let (folder, store) = (operations.fixture.folder(), operations.fixture.store.clone());
    operations.run_at(OperationPoint::DiskChanged, move || {
        assert!(!folder.join("old.txt").exists() && folder.join("docs/new.txt").is_file(), "the disk first");
        let (row, records) = store.call_blocking(move |store| Ok((store.item_store().reader().find_path(file)?, store.item_store().list_place_change_records()?))).unwrap();
        assert_eq!(row, Some("old.txt".into()), "the row second");
        let (from, to) = (ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "old.txt".into() }, ItemPlace { parent_local_item_id: docs, name: "new.txt".into() });
        assert_eq!(records.into_iter().map(|(_, record)| record).collect::<Vec<_>>(), [PlaceChangeRecord::MoveEntry { item: file, from, to }]);
    });
    assert_eq!(operations.rename("old.txt", "docs/new.txt"), Ok(()));
    operations.clear_hook();
    assert_eq!(operations.id("docs/new.txt"), file, "the item is the same");
    assert_eq!(operations.fixture.read_disk_file("docs/new.txt").as_deref(), Some(&b"mine"[..]));
    assert!(!operations.fixture.disk_path("old.txt").exists());
    // The directory above the new place was made, and marked as the directory of OneDrive it is.
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, Some(onedrive_id_of("docs")));
    assert_eq!(operations.list_records(), []);
    // An item of OneDrive moved here is a move to send; a new name the rules ignore is flagged.
    let copy = operations.fixture.show_onedrive_file("doc.txt", b"the copy").local_item_id;
    operations.fixture.land_copy(copy, b"the copy");
    assert_eq!(operations.rename("doc.txt", "docs/doc.tmp"), Ok(()));
    let moved = operations.fixture.find_item(copy).unwrap();
    assert!(moved.is_name_ignored && moved.unsent_since.is_some() && moved.file_sync_state() == Some(FileSyncState::Synced));
    assert_eq!(operations.rename_as("docs/doc.tmp", "back.txt", RenameMode::NoReplace), Ok(()));
    assert!(!operations.fixture.find_item(copy).unwrap().is_name_ignored);
    // The same name again is nothing to do.
    assert_eq!(operations.rename("back.txt", "back.txt"), Ok(()));
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_renamed_takes_what_is_in_it_with_one_row_written() {
    let operations = OperationsFixture::new();
    let directory = operations.fixture.make_own_directory("old").local_item_id;
    let inside = operations.fixture.make_own_file("old/inside.txt", b"inside");
    operations.fixture.make_own_directory("old/deeper");
    let deeper = operations.fixture.make_own_file("old/deeper/file.txt", b"deeper");
    assert_eq!(operations.rename("old", "new"), Ok(()));
    assert_eq!(operations.id("new"), directory);
    // The rows below it are as they were: they name their directory by its id.
    assert_eq!(operations.fixture.find_item(inside.local_item_id), Some(inside));
    assert_eq!(operations.fixture.find_item(deeper.local_item_id), Some(deeper));
    assert_eq!(operations.fixture.read_disk_file("new/deeper/file.txt").as_deref(), Some(&b"deeper"[..]));
    assert!(!operations.fixture.disk_path("old").exists());
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
    // A directory of OneDrive with no directory on disk moves in its row alone.
    let remote = operations.fixture.show_onedrive_directory("remote").local_item_id;
    operations.fixture.show_onedrive_file("remote/file.txt", b"in OneDrive");
    assert_eq!(operations.rename("remote", "new/remote"), Ok(()));
    assert_eq!(operations.id("new/remote"), remote);
    assert!(operations.fixture.find_item_at("new/remote/file.txt").is_some() && !operations.fixture.disk_path("new/remote").exists());
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_saved_over_another_takes_its_binding_and_the_other_row_goes() {
    let operations = OperationsFixture::new();
    let original = operations.fixture.show_onedrive_file("doc.txt", b"as it was").local_item_id;
    operations.fixture.land_copy(original, b"as it was");
    let saved = operations.fixture.make_own_file("doc.new", b"saved here").local_item_id;
    // With the disk changed and the row not written: the original is set aside under its
    // item's name, and the saved file has its place.
    let folder = operations.fixture.folder();
    operations.run_at(OperationPoint::DiskChanged, move || {
        assert_eq!(std::fs::read(folder.join(format!(".konedrive-aside-{original}"))).unwrap(), b"as it was");
        assert_eq!(std::fs::read(folder.join("doc.txt")).unwrap(), b"saved here");
    });
    assert_eq!(operations.rename("doc.new", "doc.txt"), Ok(()));
    let row = operations.fixture.find_item_at("doc.txt").unwrap();
    assert_eq!(row.local_item_id, saved, "the item the kernel holds is the saved file's");
    assert_eq!(row.onedrive_item, Some(ItemFixture::onedrive_reference_of("doc.txt")), "with the binding of the file it was saved over");
    assert_eq!((row.synced_content.clone(), row.file_sync_state()), (Some(ItemFixture::onedrive_content_of(b"as it was")), Some(FileSyncState::ModifiedHere)));
    assert_eq!(operations.fixture.find_item(original), None, "the other row went");
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"saved here"[..]));
    assert_eq!(operations.fixture.read_disk_marks("doc.txt").onedrive_item_id, Some(onedrive_id_of("doc.txt")), "marked as the copy of that item");
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]), "what was displaced is removed for good after the row");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_saved_by_a_rename_over_the_original_is_one_item_with_its_binding_and_new_content() {
    let operations = OperationsFixture::new();
    let original = operations.fixture.show_onedrive_file("report.txt", b"the first draft").local_item_id;
    operations.fixture.land_copy(original, b"the first draft");
    // As an editor saves: a temporary file made, written, closed, and renamed over.
    let temporary = operations.make(".report.txt.swp", MadeKind::FileOpenedForWriting).unwrap();
    operations.write_through(temporary.fh, b"the second draft, longer");
    operations.close(temporary.fh);
    assert_eq!(operations.rename(".report.txt.swp", "report.txt"), Ok(()));
    let files: Vec<Item> = operations.fixture.list_items().into_iter().filter(|row| row.kind == ItemKind::File).collect();
    let [row] = &files[..] else { panic!("one item is left: {files:?}") };
    assert_eq!(row.onedrive_item, Some(ItemFixture::onedrive_reference_of("report.txt")), "its binding is kept: OneDrive sees one item whose content changed");
    assert_eq!(row.onedrive.as_ref().unwrap().content, Some(ItemFixture::onedrive_content_of(b"the first draft")));
    assert!(matches!(row.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 24, .. })), "the content is the saved one: {row:?}");
    assert_eq!(row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(operations.fixture.read_disk_file("report.txt").as_deref(), Some(&b"the second draft, longer"[..]));
    assert!(!operations.fixture.disk_path(".report.txt.swp").exists());
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_renamed_over_a_file_that_is_not_on_disk_moves_with_nothing_set_aside() {
    let operations = OperationsFixture::new();
    // The displaced file is not on disk, and neither is the directory it is in.
    operations.fixture.show_onedrive_directory("docs");
    let remote = operations.fixture.show_onedrive_file("docs/remote.txt", b"in OneDrive").local_item_id;
    let saved = operations.fixture.make_own_file("saved.txt", b"saved here").local_item_id;
    let folder = operations.fixture.folder();
    operations.run_at(OperationPoint::DiskChanged, move || assert!(!std::fs::read_dir(&folder).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".konedrive-aside-")), "nothing was there to set aside"));
    assert_eq!(operations.rename("saved.txt", "docs/remote.txt"), Ok(()));
    let row = operations.fixture.find_item_at("docs/remote.txt").unwrap();
    assert_eq!((row.local_item_id, row.onedrive_item.clone()), (saved, Some(ItemFixture::onedrive_reference_of("docs/remote.txt"))));
    assert_eq!(operations.fixture.find_item(remote), None);
    assert_eq!(operations.fixture.read_disk_file("docs/remote.txt").as_deref(), Some(&b"saved here"[..]));
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, Some(onedrive_id_of("docs")), "the directory above was made, marked");
    assert_eq!(operations.fixture.read_disk_marks("docs/remote.txt").onedrive_item_id, Some(onedrive_id_of("docs/remote.txt")));
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_not_on_disk_is_renamed_with_no_disk_change() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&reached);
    operations.run_at(OperationPoint::RecordWritten, move || seen.store(true, std::sync::atomic::Ordering::SeqCst));
    assert_eq!(operations.rename("remote.txt", "renamed.txt"), Ok(()));
    assert_eq!(operations.id("renamed.txt"), remote);
    // Over another file that is not on disk: the rows alone again.
    let other = operations.fixture.show_onedrive_file("other.txt", b"also in OneDrive").local_item_id;
    assert_eq!(operations.rename("renamed.txt", "other.txt"), Ok(()));
    assert!(!reached.load(std::sync::atomic::Ordering::SeqCst), "no name changed on disk: no record");
    assert_eq!(operations.id("other.txt"), remote);
    // Both are items of OneDrive: each keeps its binding, and the displaced one is a removal to send.
    assert!(operations.fixture.find_item(other).is_some_and(|row| row.local.is_none() && row.onedrive_item.is_some()));
    assert!(std::fs::read_dir(operations.fixture.folder()).unwrap().next().is_none(), "nothing is on disk");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_not_on_disk_renamed_over_a_file_on_disk_sets_that_one_aside() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    let mine = operations.fixture.make_own_file("mine.txt", b"mine").local_item_id;
    let (folder, store) = (operations.fixture.folder(), operations.fixture.store.clone());
    operations.run_at(OperationPoint::DiskChanged, move || {
        assert_eq!(std::fs::read(folder.join(format!(".konedrive-aside-{mine}"))).unwrap(), b"mine");
        let records = store.call_blocking(|store| store.item_store().list_place_change_records()).unwrap();
        assert!(matches!(&records[..], [(_, PlaceChangeRecord::SetAside { item, .. })] if *item == mine), "{records:?}");
    });
    assert_eq!(operations.rename("remote.txt", "mine.txt"), Ok(()));
    assert_eq!(operations.id("mine.txt"), remote);
    assert_eq!(operations.fixture.find_item(mine), None);
    assert!(!operations.fixture.disk_path("mine.txt").exists(), "the file there now is not on disk");
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_is_not_renamed_into_what_is_below_it() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_directory("top");
    operations.fixture.make_own_directory("top/below");
    assert_eq!(operations.rename("top", "top/inside"), Err(Errno::EINVAL));
    assert_eq!(operations.rename("top", "top/below/inside"), Err(Errno::EINVAL));
    // A name that only begins like it is another directory.
    operations.fixture.make_own_directory("top-other");
    assert_eq!(operations.rename("top", "top-other/inside"), Ok(()));
    assert!(operations.fixture.disk_path("top-other/inside/below").is_dir());
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_directory_over_a_directory_that_is_not_empty_is_refused() {
    let operations = OperationsFixture::new();
    let moved = operations.fixture.make_own_directory("moved").local_item_id;
    operations.fixture.make_own_directory("full");
    operations.fixture.make_own_file("full/inside.txt", b"inside");
    assert_eq!(operations.rename("moved", "full"), Err(Errno::ENOTEMPTY));
    assert!(operations.fixture.disk_path("moved").is_dir() && operations.fixture.disk_path("full/inside.txt").is_file());
    // Over an empty one it goes, and the empty one's row with it.
    let empty = operations.fixture.make_own_directory("empty").local_item_id;
    assert_eq!(operations.rename("moved", "empty"), Ok(()));
    assert_eq!((operations.id("empty"), operations.fixture.find_item(empty)), (moved, None));
    assert!(operations.fixture.disk_path("empty").is_dir() && !operations.fixture.disk_path("moved").exists());
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    // One the rows say is empty and the disk does not: the place is held.
    operations.fixture.make_own_directory("held");
    operations.fixture.write_disk_file("held/leftover.txt", b"what no row says");
    assert_eq!(operations.rename("empty", "held"), Err(Errno::EEXIST));
    assert_eq!(operations.fixture.read_disk_file("held/leftover.txt").as_deref(), Some(&b"what no row says"[..]));
    assert_eq!(operations.id("empty"), moved);
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_file_over_a_directory_and_a_directory_over_a_file_are_refused() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_file("file.txt", b"file");
    operations.fixture.make_own_directory("directory");
    assert_eq!(operations.rename("file.txt", "directory"), Err(Errno::EISDIR));
    assert_eq!(operations.rename("directory", "file.txt"), Err(Errno::ENOTDIR));
    assert_eq!(operations.rename_as("file.txt", "directory", RenameMode::NoReplace), Err(Errno::EEXIST));
    assert_eq!(operations.rename("nothing", "new"), Err(Errno::ENOENT));
    let file = operations.id("file.txt");
    assert_eq!(operations.run(move |operations| operations.rename(LocalItemId::ROOT, OsStr::new("directory"), file, OsStr::new("x"), RenameMode::Replace)), Err(Errno::ENOENT), "a node that is no directory");
    assert!(operations.fixture.disk_path("file.txt").is_file() && operations.fixture.disk_path("directory").is_dir());
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_rename_to_a_name_of_the_daemons_own_is_refused() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_file("file.txt", b"file");
    for name in [".konedrive-holding", ".konedrive-new-7", ".konedrive-aside-7", ".konedrive-swap-7"] {
        assert_eq!(operations.rename("file.txt", name), Err(Errno::EPERM), "{name}");
    }
    assert!(operations.fixture.disk_path("file.txt").is_file());
    assert_eq!(operations.list_records(), []);
}

#[test]
fn an_exchange_of_two_names_is_refused() {
    let operations = OperationsFixture::new();
    let (first, second) = (operations.fixture.make_own_file("first.txt", b"first"), operations.fixture.make_own_file("second.txt", b"second"));
    assert_eq!(operations.rename_as("first.txt", "second.txt", RenameMode::Exchange), Err(Errno::EINVAL));
    assert_eq!((operations.fixture.find_item_at("first.txt"), operations.fixture.find_item_at("second.txt")), (Some(first), Some(second)));
    assert_eq!(operations.fixture.read_disk_file("second.txt").as_deref(), Some(&b"second"[..]));
}

#[test]
fn an_item_of_onedrive_moved_into_a_directory_that_is_left_here_is_taken() {
    let operations = OperationsFixture::new();
    operations.make("cache", MadeKind::Directory).unwrap();
    let copy = operations.fixture.show_onedrive_file("doc.txt", b"the copy").local_item_id;
    operations.fixture.land_copy(copy, b"the copy");
    assert_eq!(operations.rename("doc.txt", "cache/doc.txt"), Ok(()), "today: EXDEV");
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!(operations.id("cache/doc.txt"), copy);
    assert_eq!(row.onedrive_item, Some(ItemFixture::onedrive_reference_of("doc.txt")), "it stays an item of OneDrive");
    assert!(row.unsent_since.is_some(), "whose move is the plan's to judge");
    assert!(!operations.fixture.read_rows(move |reader| reader.is_ignored(copy)), "an item OneDrive has is never left here");
    assert_eq!(operations.fixture.read_disk_file("cache/doc.txt").as_deref(), Some(&b"the copy"[..]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_rename_to_a_place_the_disk_holds_though_no_row_says_so_is_refused_and_replaces_nothing() {
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("file.txt", b"file").local_item_id;
    operations.fixture.write_disk_file("leftover.txt", b"what no row says");
    assert_eq!(operations.rename("file.txt", "leftover.txt"), Err(Errno::EEXIST));
    assert_eq!(operations.fixture.read_disk_file("leftover.txt").as_deref(), Some(&b"what no row says"[..]));
    assert_eq!(operations.fixture.read_disk_file("file.txt").as_deref(), Some(&b"file"[..]));
    assert_eq!(operations.id("file.txt"), file);
    assert_eq!((operations.list_records(), operations.take_downs()), (vec![], vec![]), "the record of a change the disk refused is taken");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_row_refused_after_the_disk_renames_back_and_restores_what_was_displaced() {
    // To a free name: a row takes the place meanwhile.
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("file.txt", b"file").local_item_id;
    operations.fixture.show_onedrive_directory("docs");
    operations.take_place_at(OperationPoint::DiskChanged, "docs", "new.txt");
    assert_eq!(operations.rename("file.txt", "docs/new.txt"), Err(Errno::EIO));
    assert_eq!(operations.fixture.read_disk_file("file.txt").as_deref(), Some(&b"file"[..]), "moved back");
    assert!(!operations.fixture.disk_path("docs").exists(), "and the directory that was made above the new place is gone");
    assert_eq!(operations.fixture.find_item_at("file.txt").map(|row| row.local_item_id), Some(file));
    assert_eq!((operations.list_aside_entries(), operations.list_records(), operations.take_downs()), (vec![], vec![], vec![]));

    // Over an entry: the displaced directory gets something in it meanwhile, by its rows.
    let operations = OperationsFixture::new();
    let moved = operations.fixture.make_own_directory("moved").local_item_id;
    let displaced = operations.fixture.show_onedrive_directory("displaced").local_item_id;
    operations.fixture.make_directory_on_disk(displaced);
    let store = operations.fixture.store.clone();
    operations.run_at(OperationPoint::DiskChanged, move || {
        let place = ItemPlace { parent_local_item_id: displaced, name: "came-meanwhile".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
    });
    assert_eq!(operations.rename("moved", "displaced"), Err(Errno::EIO));
    assert!(operations.fixture.disk_path("moved").is_dir() && operations.fixture.disk_path("displaced").is_dir());
    assert_eq!(operations.fixture.read_disk_marks("displaced").onedrive_item_id, Some(onedrive_id_of("displaced")), "what was displaced is back under its name, as it was");
    assert_eq!(operations.fixture.read_disk_marks("moved").onedrive_item_id, None);
    assert_eq!((operations.id("moved"), operations.id("displaced")), (moved, displaced));
    assert_eq!((operations.list_aside_entries(), operations.list_records(), operations.take_downs()), (vec![], vec![], vec![]));
}

#[test]
fn a_move_that_cannot_be_taken_back_asks_for_the_take_down_and_keeps_its_record() {
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("file.txt", b"file").local_item_id;
    // The row is refused, and by then something is at the old name on disk: the entry
    // cannot move back, and nothing is replaced by trying.
    let (store, in_the_way) = (operations.fixture.store.clone(), operations.fixture.disk_path("file.txt"));
    operations.run_at(OperationPoint::DiskChanged, move || {
        let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "new.txt".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
        std::fs::write(&in_the_way, b"in the way").unwrap();
    });
    assert_eq!(operations.rename("file.txt", "new.txt"), Err(Errno::EIO));
    assert_eq!(operations.take_downs(), [TakeDownCause::UndoFailed { path: "new.txt".into() }]);
    assert!(!operations.fixture.access_mode_switch.is_read_write());
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::MoveEntry { item, .. }] if item == file), "the record stands for the check at start");
    assert_eq!(operations.fixture.read_disk_file("file.txt").as_deref(), Some(&b"in the way"[..]));
    assert_eq!(operations.fixture.read_disk_file("new.txt").as_deref(), Some(&b"file"[..]));
}

#[test]
fn a_file_open_for_writing_is_renamed_and_its_writer_keeps_its_file() {
    let operations = OperationsFixture::new();
    let made = operations.make("open.txt", MadeKind::FileOpenedForWriting).unwrap();
    let id = made.item.local_item_id;
    assert_eq!(operations.rename("open.txt", "renamed.txt"), Ok(()));
    assert!(operations.fixture.item_handles.is_open_for_writing(id));
    operations.write_through(made.fh, b"written after the rename");
    operations.close(made.fh);
    assert_eq!(operations.fixture.read_disk_file("renamed.txt").as_deref(), Some(&b"written after the rename"[..]));
    let row = operations.fixture.find_item_at("renamed.txt").unwrap();
    assert_eq!((row.local_item_id, row.local.unwrap().content.unwrap().size(), row.content_before_write), (id, 24, None), "the close is written to the item, wherever it is");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_with_a_file_open_for_writing_in_it_is_renamed_and_the_close_is_written() {
    let operations = OperationsFixture::new();
    operations.fixture.show_onedrive_directory("old");
    let copy = operations.fixture.show_onedrive_file("old/doc.txt", b"the copy").local_item_id;
    operations.fixture.land_copy(copy, b"the copy");
    let opened = operations.run(move |operations| operations.open_to_write(copy)).unwrap();
    assert_eq!(operations.rename("old", "new"), Ok(()));
    assert_eq!(operations.id("new/doc.txt"), copy);
    operations.write_through(opened.fh, b"changed in the renamed directory");
    operations.close(opened.fh);
    assert_eq!(operations.fixture.read_disk_file("new/doc.txt").as_deref(), Some(&b"changed in the renamed directory"[..]));
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!((row.local.as_ref().unwrap().content.as_ref().unwrap().size(), row.content_before_write.clone(), row.file_sync_state()), (32, None, Some(FileSyncState::ModifiedHere)));
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}
