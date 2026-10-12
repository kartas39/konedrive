//! Remove: the decision's cases, and each removal over the fixture.

use std::ffi::OsStr;
use std::sync::Arc;

use konedrive_tree::{ItemPlace, LocalVersionChange};

use super::*;
use crate::mount::item_fixture::{onedrive_id_of, ItemFixture};
use crate::mount::item_write_operations::tests::{make_row, take_off_disk, OperationPoint, OperationsFixture};
use crate::mount::item_write_operations::MadeKind;
use crate::mount::mount_take_down::TakeDownCause;

fn case(item: Option<Item>, asked_kind: ItemKind) -> RemoveCase {
    RemoveCase { directory: Some(make_row(2, ItemKind::Directory)), path: item.as_ref().map(|_| "d/x".into()), item, asked_kind, is_empty: true, onedrive_items_never_shown: konedrive_tree::ItemsNeverShown::Nothing }
}

#[test]
fn each_case_of_a_removal_is_decided() {
    let (file, directory) = (make_row(5, ItemKind::File), make_row(6, ItemKind::Directory));
    let (file_id, directory_id) = (file.local_item_id, directory.local_item_id);
    assert_eq!(decide_remove(&case(Some(file.clone()), ItemKind::File)), Ok(RemoveDecision::SetAsideOnDisk { item: file_id }));
    assert_eq!(decide_remove(&case(Some(take_off_disk(file.clone())), ItemKind::File)), Ok(RemoveDecision::RowAlone { item: file_id }), "a file that is not on disk");
    // A directory may be on disk whatever its row says.
    assert_eq!(decide_remove(&case(Some(directory.clone()), ItemKind::Directory)), Ok(RemoveDecision::SetAsideOnDisk { item: directory_id }));
    for asked_kind in [ItemKind::File, ItemKind::Directory] {
        assert_eq!(decide_remove(&case(None, asked_kind)), Err(Errno::ENOENT), "nothing at the name");
        assert_eq!(decide_remove(&RemoveCase { directory: None, ..case(Some(file.clone()), asked_kind) }), Err(Errno::ENOENT), "no directory");
    }
    assert_eq!(decide_remove(&case(Some(directory.clone()), ItemKind::File)), Err(Errno::EISDIR), "unlink of a directory");
    assert_eq!(decide_remove(&case(Some(file), ItemKind::Directory)), Err(Errno::ENOTDIR), "rmdir of a file");
    assert_eq!(decide_remove(&RemoveCase { is_empty: false, ..case(Some(directory.clone()), ItemKind::Directory) }), Err(Errno::ENOTEMPTY));
    // Today's order: the kind is asked before what the directory holds.
    assert_eq!(decide_remove(&RemoveCase { is_empty: false, ..case(Some(directory), ItemKind::File) }), Err(Errno::EISDIR));
}

#[test]
fn a_file_removed_is_set_aside_then_gone_from_its_row_then_gone_from_disk() {
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("mine.txt", b"mine").local_item_id;
    // With the disk changed and the row not written: the entry is set aside under its
    // item's name, on record, and its row still stands.
    let (folder, store) = (operations.fixture.folder(), operations.fixture.store.clone());
    operations.run_at(OperationPoint::DiskChanged, move || {
        assert!(!folder.join("mine.txt").exists());
        assert_eq!(std::fs::read(folder.join(format!(".konedrive-aside-{file}"))).unwrap(), b"mine");
        let (row, records) = store.call_blocking(move |store| Ok((store.item_store().reader().find_item(file)?, store.item_store().list_place_change_records()?))).unwrap();
        assert!(row.is_some_and(|row| row.local.is_some()), "the row is written after the disk");
        let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "mine.txt".into() };
        assert_eq!(records.into_iter().map(|(_, record)| record).collect::<Vec<_>>(), [PlaceChangeRecord::SetAside { item: file, place, aside_name: format!(".konedrive-aside-{file}").into() }]);
    });
    assert_eq!(operations.remove("mine.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.fixture.find_item(file), None, "the row of a file OneDrive never had goes");
    assert!(!operations.fixture.disk_path("mine.txt").exists());
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]), "removed for good after the row, and no record left");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_of_onedrive_removed_keeps_its_row_as_a_removal_to_send() {
    let operations = OperationsFixture::new();
    let copy = operations.fixture.show_onedrive_file("doc.txt", b"the copy").local_item_id;
    operations.fixture.land_copy(copy, b"the copy");
    assert_eq!(operations.remove("doc.txt", ItemKind::File), Ok(()));
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!((row.local.clone(), row.onedrive_item.clone()), (None, Some(ItemFixture::onedrive_reference_of("doc.txt"))));
    assert!(row.synced_place.is_some() && row.unsent_since.is_some(), "what was agreed on stays: the unsent removal");
    assert!(!operations.fixture.disk_path("doc.txt").exists());
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_not_on_disk_is_removed_with_no_disk_change() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&reached);
    operations.run_at(OperationPoint::RecordWritten, move || seen.store(true, std::sync::atomic::Ordering::SeqCst));
    assert_eq!(operations.remove("remote.txt", ItemKind::File), Ok(()));
    assert!(!reached.load(std::sync::atomic::Ordering::SeqCst), "nothing of a name changes on disk: no record");
    assert!(operations.fixture.find_item(remote).is_some_and(|row| row.local.is_none() && row.unsent_since.is_some()));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_the_folder_shows_something_in_is_not_removed() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_directory("mine");
    operations.fixture.make_own_file("mine/inside.txt", b"inside");
    // Also when what it shows is not on disk: the rows say what is in it.
    operations.fixture.show_onedrive_directory("docs");
    operations.fixture.show_onedrive_file("docs/remote.txt", b"in OneDrive");
    for directory in ["mine", "docs"] {
        assert_eq!(operations.remove(directory, ItemKind::Directory), Err(Errno::ENOTEMPTY), "{directory}");
        assert!(operations.fixture.find_item_at(directory).is_some());
    }
    assert_eq!(operations.fixture.read_disk_file("mine/inside.txt").as_deref(), Some(&b"inside"[..]));
    assert_eq!(operations.list_records(), []);
    // Emptied, it goes.
    operations.remove("mine/inside.txt", ItemKind::File).unwrap();
    assert_eq!(operations.remove("mine", ItemKind::Directory), Ok(()));
    assert!(operations.fixture.find_item_at("mine").is_none() && !operations.fixture.disk_path("mine").exists());
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_of_onedrive_with_no_directory_on_disk_is_removed_in_its_row() {
    let operations = OperationsFixture::new();
    let docs = operations.fixture.show_onedrive_directory("docs").local_item_id;
    assert!(!operations.fixture.disk_path("docs").exists());
    assert_eq!(operations.remove("docs", ItemKind::Directory), Ok(()));
    assert!(operations.fixture.find_item(docs).is_some_and(|row| row.local.is_none() && row.onedrive_item.is_some() && row.unsent_since.is_some()));
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    // And one that is on disk goes from there.
    let kept = operations.fixture.show_onedrive_directory("kept").local_item_id;
    operations.fixture.make_directory_on_disk(kept);
    assert_eq!(operations.remove("kept", ItemKind::Directory), Ok(()));
    assert!(!operations.fixture.disk_path("kept").exists());
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
}

#[test]
fn a_file_is_not_removed_by_rmdir_and_a_directory_not_by_unlink() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_file("file.txt", b"file");
    operations.fixture.make_own_directory("directory");
    assert_eq!(operations.remove("file.txt", ItemKind::Directory), Err(Errno::ENOTDIR));
    assert_eq!(operations.remove("directory", ItemKind::File), Err(Errno::EISDIR));
    assert_eq!(operations.remove("nothing", ItemKind::File), Err(Errno::ENOENT));
    assert_eq!(operations.remove("nothing", ItemKind::Directory), Err(Errno::ENOENT));
    let file = operations.id("file.txt");
    assert_eq!(operations.run(move |operations| operations.remove(file, OsStr::new("x"), ItemKind::File)), Err(Errno::ENOENT), "a node that is no directory");
    assert!(operations.fixture.disk_path("file.txt").is_file() && operations.fixture.disk_path("directory").is_dir());
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

/// The folder shows something in `directory` from `point` on, behind the operation's
/// back: the store then refuses the directory's removal.
fn put_row_into(operations: &OperationsFixture, point: OperationPoint, directory: &str) {
    let (store, directory) = (operations.fixture.store.clone(), operations.id(directory));
    operations.run_at(point, move || {
        let place = ItemPlace { parent_local_item_id: directory, name: "came-meanwhile".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
    });
}

#[test]
fn a_row_refused_after_the_disk_puts_the_entry_back() {
    let operations = OperationsFixture::new();
    let docs = operations.fixture.show_onedrive_directory("docs").local_item_id;
    operations.fixture.make_directory_on_disk(docs);
    put_row_into(&operations, OperationPoint::DiskChanged, "docs");
    assert_eq!(operations.remove("docs", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert!(operations.fixture.disk_path("docs").is_dir(), "the directory is back under its name");
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, Some(onedrive_id_of("docs")), "with its mark");
    assert!(operations.fixture.find_item(docs).is_some_and(|row| row.local.is_some()), "and its row stands");
    assert_eq!((operations.list_aside_entries(), operations.list_records(), operations.take_downs()), (vec![], vec![], vec![]));
}

#[test]
fn an_entry_that_cannot_be_put_back_asks_for_the_take_down_and_keeps_its_record() {
    let operations = OperationsFixture::new();
    let directory = operations.fixture.make_own_directory("mine").local_item_id;
    // The row is refused, and by then something is at the directory's name on disk: the
    // entry that was set aside cannot go back.
    let (store, in_the_way) = (operations.fixture.store.clone(), operations.fixture.disk_path("mine"));
    operations.run_at(OperationPoint::DiskChanged, move || {
        let place = ItemPlace { parent_local_item_id: directory, name: "came-meanwhile".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
        std::fs::write(&in_the_way, b"in the way").unwrap();
    });
    assert_eq!(operations.remove("mine", ItemKind::Directory), Err(Errno::EIO));
    assert_eq!(operations.take_downs(), [TakeDownCause::UndoFailed { path: "mine".into() }]);
    assert!(!operations.fixture.access_mode_switch.is_read_write());
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::SetAside { item, .. }] if item == directory), "the record stands for the check at start");
    assert_eq!(operations.list_aside_entries(), [format!(".konedrive-aside-{directory}")], "and the entry is still set aside, for the check to put back");
}

#[test]
fn a_directory_that_holds_what_no_row_says_stays_on_disk_unmarked_and_its_row_goes() {
    let operations = OperationsFixture::new();
    let docs = operations.fixture.show_onedrive_directory("docs").local_item_id;
    operations.fixture.make_directory_on_disk(docs);
    operations.fixture.write_disk_file("docs/leftover.txt", b"what no row says");
    assert_eq!(operations.remove("docs", ItemKind::Directory), Ok(()));
    assert!(operations.fixture.find_item(docs).is_some_and(|row| row.local.is_none()), "the removal is to be sent");
    assert_eq!(operations.fixture.read_disk_file("docs/leftover.txt").as_deref(), Some(&b"what no row says"[..]), "nothing in it is removed");
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, None, "it is no directory of OneDrive any more");
    assert_eq!((operations.list_aside_entries(), operations.list_records()), (vec![], vec![]));
    // A directory made at the name takes it.
    assert!(operations.make("docs", MadeKind::Directory).is_ok());
    // A refused row puts the mark back.
    let kept = operations.fixture.show_onedrive_directory("kept").local_item_id;
    operations.fixture.make_directory_on_disk(kept);
    operations.fixture.write_disk_file("kept/leftover.txt", b"what no row says");
    put_row_into(&operations, OperationPoint::DiskChanged, "kept");
    assert_eq!(operations.remove("kept", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(operations.fixture.read_disk_marks("kept").onedrive_item_id, Some(onedrive_id_of("kept")));
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_file_open_for_writing_is_removed_and_its_writers_close_writes_nothing() {
    let operations = OperationsFixture::new();
    let made = operations.make("open.txt", MadeKind::FileOpenedForWriting).unwrap();
    let id = made.item.local_item_id;
    assert_eq!(operations.remove("open.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.fixture.find_item(id), None);
    assert!(!operations.fixture.disk_path("open.txt").exists());
    // The writer keeps its file, writes it, and its close changes no row.
    operations.write_through(made.fh, b"written after the removal");
    let rows = operations.fixture.list_items();
    operations.close(made.fh);
    assert_eq!(operations.fixture.list_items(), rows);
    assert!(!operations.fixture.item_handles.is_open_for_writing(id));
    // The same for a file of OneDrive, whose row stays as the removal: no content comes back.
    let copy = operations.fixture.show_onedrive_file("doc.txt", b"the copy").local_item_id;
    operations.fixture.land_copy(copy, b"the copy");
    let opened = operations.run(move |operations| operations.open_to_write(copy)).unwrap();
    assert_eq!(operations.remove("doc.txt", ItemKind::File), Ok(()));
    operations.write_through(opened.fh, b"written after the removal");
    let rows = operations.fixture.list_items();
    operations.close(opened.fh);
    assert_eq!(operations.fixture.list_items(), rows);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_with_what_was_never_shown_in_it_is_decided_not_empty_for_an_rmdir_alone() {
    use konedrive_tree::ItemsNeverShown;
    let (file, directory) = (make_row(5, ItemKind::File), make_row(6, ItemKind::Directory));
    let with = |never_shown, item: &Item, asked_kind| RemoveCase { onedrive_items_never_shown: never_shown, ..case(Some(item.clone()), asked_kind) };
    assert_eq!(decide_remove(&with(ItemsNeverShown::Nothing, &directory, ItemKind::Directory)), Ok(RemoveDecision::SetAsideOnDisk { item: directory.local_item_id }));
    for never_shown in [ItemsNeverShown::NotShownYet, ItemsNeverShown::CannotBeShown] {
        assert_eq!(decide_remove(&with(never_shown, &directory, ItemKind::Directory)), Err(Errno::ENOTEMPTY), "{never_shown:?}");
        // The kind is asked first, as for a directory the folder shows something in.
        assert_eq!(decide_remove(&with(never_shown, &directory, ItemKind::File)), Err(Errno::EISDIR), "{never_shown:?}");
        assert_eq!(decide_remove(&with(never_shown, &file, ItemKind::Directory)), Err(Errno::ENOTDIR), "{never_shown:?}");
        assert_eq!(decide_remove(&RemoveCase { directory: None, ..with(never_shown, &directory, ItemKind::Directory) }), Err(Errno::ENOENT), "{never_shown:?}");
    }
}

/// The operations of a fixture whose report keeps the activity of the fixture's folder,
/// with that activity.
fn operations_with_activity() -> (OperationsFixture, Arc<crate::status::activity::Activity>) {
    use crate::status::report::Report;
    use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
    let mut kept = None;
    let operations = OperationsFixture::with_report(|fixture| {
        let mut snapshot = SyncSnapshot::default();
        snapshot.folder.root_path = fixture.folder().display().to_string();
        let state = SyncStateHandle::new(snapshot);
        let report = Report { activity: Arc::new(crate::status::activity::Activity::new(state.clone())), ..Report::nowhere() };
        kept = Some(Arc::clone(&report.activity));
        report
    });
    (operations, kept.unwrap())
}

/// The paths the user was told cannot be removed here, oldest first.
fn list_told(activity: &crate::status::activity::Activity) -> Vec<String> {
    let mut events = activity.recent(100).unwrap();
    events.reverse();
    assert!(events.iter().all(|event| event.kind == konedrive_tree::ActivityKind::NotRemovedHere), "{events:?}");
    events.into_iter().map(|event| event.path).collect()
}

#[test]
fn a_directory_that_holds_what_cannot_be_shown_is_not_removed_and_the_user_is_told_once() {
    let (operations, activity) = operations_with_activity();
    let fixture = &operations.fixture;
    let mut announced = activity.subscribe();
    for directory in ["docs", "more"] {
        fixture.show_onedrive_directory(directory);
        fixture.list_onedrive_file_not_placed(&format!("{directory}/notes.one"), b"a notebook", konedrive_tree::SkipReason::OneNote);
    }
    fixture.show_onedrive_file("docs/shown.txt", b"shown");
    let docs = fixture.folder().join("docs").display().to_string();

    // While the folder shows something in it, it is not empty as any directory: nothing to tell.
    assert_eq!(operations.remove("docs", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(list_told(&activity), Vec::<String>::new());
    // The shown file is removed as asked; the directory stays, and the user is told why.
    assert_eq!(operations.remove("docs/shown.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.remove("docs", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(list_told(&activity), [docs.clone()]);
    assert_eq!(announced.try_recv().map(|event| (event.kind, event.path)).ok(), Some((konedrive_tree::ActivityKind::NotRemovedHere, docs.clone())), "announced as it is recorded");
    assert!(fixture.find_item_at("docs").is_some_and(|row| row.local.is_some() && row.unsent_since.is_none()), "the directory is shown as before, and nothing of it is unsent");
    // A program that tries again is not told again.
    assert_eq!(operations.remove("docs", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(list_told(&activity), [docs.clone()]);
    assert!(announced.try_recv().is_err());

    // An unlink of such a directory is refused as any directory's, which says why: nothing is told, and the rmdir after it tells.
    assert_eq!(operations.remove("more", ItemKind::File), Err(Errno::EISDIR));
    assert_eq!(list_told(&activity), [docs.clone()]);
    assert_eq!(operations.remove("more", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(list_told(&activity), [docs, fixture.folder().join("more").display().to_string()]);
    assert_eq!(operations.list_records(), [], "nothing was begun on disk");
}

#[test]
fn a_directory_with_an_item_not_shown_yet_is_not_removed_and_nothing_is_told() {
    let (operations, activity) = operations_with_activity();
    let fixture = &operations.fixture;
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_directory("docs/sub");
    // At any depth: a listing brought a file no step has shown yet.
    fixture.list_onedrive_file("docs/sub/waits.txt", b"new in OneDrive");
    assert_eq!(operations.remove("docs/sub", ItemKind::Directory), Err(Errno::ENOTEMPTY));
    assert_eq!(operations.remove("docs/sub", ItemKind::File), Err(Errno::EISDIR));
    assert!(fixture.find_item_at("docs/sub").is_some());
    assert_eq!(list_told(&activity), Vec::<String>::new(), "\"cannot be shown here\" would be false: it appears by itself");
    // Once OneDrive has it no more, the directory goes.
    fixture.list_onedrive_removal("docs/sub/waits.txt");
    assert_eq!(operations.remove("docs/sub", ItemKind::Directory), Ok(()));
    assert_eq!(operations.remove("docs", ItemKind::Directory), Ok(()));
    assert_eq!(list_told(&activity), Vec::<String>::new());
}

#[test]
fn an_item_an_upload_left_in_a_directory_does_not_keep_the_directory() {
    let (operations, activity) = operations_with_activity();
    let fixture = &operations.fixture;
    fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("docs/report.txt", b"sent").local_item_id;
    // An upload of the file left an item with other content, and a listing brought it: no step shows it.
    let left = fixture.list_onedrive_file("docs/report 1.txt", b"other content");
    assert_eq!(operations.remove("docs/report.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.remove("docs", ItemKind::Directory), Err(Errno::ENOTEMPTY), "before the step memory says whose the item is");
    let upload = konedrive_tree::MismatchedItem::of_answer(left.onedrive_item.clone().unwrap(), Some("c-other"), None);
    assert_eq!(fixture.write_rows(move |store| store.step_memory().remember_mismatched_item(file, &upload)), konedrive_tree::MismatchedItemStanding::LeftToRemove);
    assert_eq!(operations.remove("docs", ItemKind::Directory), Ok(()), "the folder's own leftover goes with the directory");
    assert!(fixture.find_item_at("docs").is_none());
    assert_eq!(list_told(&activity), Vec::<String>::new());
}

#[test]
fn the_last_rmdir_of_an_rm_r_of_what_onedrive_has_is_not_refused() {
    let (operations, activity) = operations_with_activity();
    let fixture = &operations.fixture;
    let docs = fixture.show_onedrive_directory("docs").local_item_id;
    let sub = fixture.show_onedrive_directory("docs/sub").local_item_id;
    let deep = fixture.show_onedrive_file("docs/sub/deep.txt", b"deep").local_item_id;
    let top = fixture.show_onedrive_file("docs/top.txt", b"top").local_item_id;
    // As `rm -r docs` asks them: what is in a directory before the directory.
    assert_eq!(operations.remove("docs/sub/deep.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.remove("docs/sub", ItemKind::Directory), Ok(()), "what was removed here, its removal unsent, is none the folder never showed");
    assert_eq!(operations.remove("docs/top.txt", ItemKind::File), Ok(()));
    assert_eq!(operations.remove("docs", ItemKind::Directory), Ok(()));
    for item in [docs, sub, deep, top] {
        let row = fixture.find_item(item).expect("a removal to send keeps its row");
        assert!(row.local.is_none() && row.unsent_since.is_some() && row.onedrive_item.is_some(), "item {item}");
    }
    assert_eq!(list_told(&activity), Vec::<String>::new());
}
