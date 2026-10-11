//! A file's content: the three decisions' cases, and each operation over the fixture.

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use konedrive_tree::FileSyncState;

use super::*;
use crate::mount::item_fixture::{onedrive_id_of, DiskMarksRead, ItemFixture, ONEDRIVE_MTIME};
use crate::mount::item_write_operations::tests::{bind, make_row, take_off_disk, OperationPoint, OperationsFixture};
use crate::mount::item_write_operations::MadeKind;
use crate::mount::place_changes::OpenMode;

#[test]
fn each_case_of_a_writers_open_is_decided() {
    for presence in [ContentPresence::OnDisk, ContentPresence::NotOnDisk] {
        assert_eq!(decide_open_change(WriterRank::Later, presence), OpenChange::Nothing, "another handle writes it already");
    }
    assert_eq!(decide_open_change(WriterRank::First, ContentPresence::OnDisk), OpenChange::OpenedForWriting);
    assert_eq!(decide_open_change(WriterRank::First, ContentPresence::NotOnDisk), OpenChange::OpenedForWritingAndOnDisk);
}

#[test]
fn each_case_of_the_last_close_is_decided() {
    let item = LocalItemId::new(7);
    let closing = |outcome| ClosingWriter { item, outcome, backing_file: Arc::new(tempfile::tempfile().unwrap()) };
    assert_eq!(decide_close_change(&closing(CloseOutcome::ContentChanged), true, 12, 34, 56), Some(LocalVersionChange::ClosedChanged { item, size: 12, mtime: 34, mtime_nsec: 56 }));
    assert_eq!(decide_close_change(&closing(CloseOutcome::ContentUntouched), true, 12, 34, 56), Some(LocalVersionChange::ClosedUnchanged { item }));
    for is_shown in [true, false] {
        assert_eq!(decide_close_change(&closing(CloseOutcome::StillOpenElsewhere), is_shown, 12, 34, 56), None, "another handle still writes");
    }
    for outcome in [CloseOutcome::ContentChanged, CloseOutcome::ContentUntouched] {
        assert_eq!(decide_close_change(&closing(outcome), false, 12, 34, 56), None, "the folder does not show the item any more");
    }
}

#[test]
fn each_case_of_what_is_downloaded_before_a_write_is_decided() {
    let on_disk = bind(make_row(7, ItemKind::File));
    let not_on_disk = take_off_disk(on_disk.clone());
    let content = not_on_disk.local.as_ref().unwrap().content.clone().unwrap();
    let download = FileToFill::Download(ItemToDownload { item: not_on_disk.local_item_id, onedrive_item: not_on_disk.onedrive_item.clone().unwrap(), content });
    for new_size in [None, Some(0), Some(5)] {
        assert_eq!(decide_file_to_fill(&on_disk, new_size), Ok(FileToFill::Nothing), "on disk");
    }
    assert_eq!(decide_file_to_fill(&not_on_disk, None), Ok(download.clone()), "opened: the content is needed");
    assert_eq!(decide_file_to_fill(&not_on_disk, Some(5)), Ok(download), "cut to a size: what stays is needed");
    assert_eq!(decide_file_to_fill(&not_on_disk, Some(0)), Ok(FileToFill::Nothing), "cut to nothing");
    let mut empty = not_on_disk.clone();
    empty.local.as_mut().unwrap().content = Some(ContentVersion::Hashed { size: 0, mtime: 1, mtime_nsec: 0, hash: None, ctag: None });
    assert_eq!(decide_file_to_fill(&empty, None), Ok(FileToFill::Nothing), "an empty file has nothing to download");
    assert_eq!(decide_file_to_fill(&Item { local: None, ..not_on_disk.clone() }, None), Err(Errno::ESTALE), "the folder does not show it");
    assert_eq!(decide_file_to_fill(&make_row(8, ItemKind::Directory), None), Err(Errno::EISDIR));
}

/// A file of OneDrive at `path` with its clean copy on disk.
fn land(operations: &OperationsFixture, path: &str, content: &[u8]) -> LocalItemId {
    let copy = operations.fixture.show_onedrive_file(path, content).local_item_id;
    operations.fixture.land_copy(copy, content);
    copy
}

fn open(operations: &OperationsFixture, item: LocalItemId) -> Result<OpenedWriter, Errno> {
    operations.run(move |operations| operations.open_to_write(item))
}

fn overwrite(operations: &OperationsFixture, item: LocalItemId) -> Result<OpenedWriter, Errno> {
    operations.run(move |operations| operations.open_to_overwrite(item))
}

fn content_of(operations: &OperationsFixture, item: LocalItemId) -> ContentVersion {
    operations.fixture.find_item(item).unwrap().local.unwrap().content.unwrap()
}

fn disk_time(operations: &OperationsFixture, path: &str) -> (i64, u32) {
    let on_disk = std::fs::metadata(operations.fixture.disk_path(path)).unwrap();
    (on_disk.mtime(), on_disk.mtime_nsec() as u32)
}

#[test]
fn the_first_writers_open_raises_the_change_count_before_the_file_is_opened() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let clean = ItemFixture::onedrive_content_of(b"the copy");
    let opened = open(&operations, copy).unwrap();
    // The row says another content already, with no byte written: the same size and
    // time, changed here; and it keeps what the content was.
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!(row.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 8, mtime: ONEDRIVE_MTIME, mtime_nsec: 0, change_count: 1 }));
    assert_eq!((row.content_before_write.clone(), row.content_presence), (Some(clean), ContentPresence::OnDisk));
    assert!(operations.fixture.item_handles.is_open_for_writing(copy));
    assert!(Arc::ptr_eq(&operations.fixture.item_handles.find_writer_file(copy).unwrap(), &opened.backing_file), "the handle writes the file that is answered");
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"the copy"[..]), "nothing of the content changed");
    assert_eq!(operations.list_records(), [], "the file is on disk: no name changes");
    // A file with no write bit, as a downloaded copy has none, is given it.
    let read_only = land(&operations, "read-only.txt", b"the copy");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(operations.fixture.disk_path("read-only.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    let writer = open(&operations, read_only).unwrap();
    operations.write_through(writer.fh, b"written");
    // What cannot be opened for writing: no row is left saying a writer opened it.
    operations.fixture.make_own_directory("d");
    assert_eq!(open(&operations, operations.id("d")).map(|opened| opened.fh), Err(Errno::EISDIR));
    assert_eq!(open(&operations, LocalItemId::new(999)).map(|opened| opened.fh), Err(Errno::ESTALE));
    let gone = land(&operations, "gone.txt", b"gone");
    std::fs::remove_file(operations.fixture.disk_path("gone.txt")).unwrap();
    assert_eq!(open(&operations, gone).map(|opened| opened.fh), Err(Errno::ENOENT));
    let row = operations.fixture.find_item(gone).unwrap();
    assert_eq!((row.local.unwrap().content, row.content_before_write), (Some(ItemFixture::onedrive_content_of(b"gone")), None), "the open that failed is taken back");
}

#[test]
fn a_second_writers_open_writes_no_row() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let first = open(&operations, copy).unwrap();
    let row = operations.fixture.find_item(copy).unwrap();
    operations.take_wake();
    let second = open(&operations, copy).unwrap();
    assert_ne!(first.fh, second.fh);
    assert_eq!(operations.fixture.find_item(copy).unwrap(), row, "the change count is raised once");
    assert!(!operations.take_wake(), "no row, no wake");
}

#[test]
fn a_file_not_on_disk_and_not_empty_is_downloaded_before_it_is_opened_for_writing() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive");
    let id = remote.local_item_id;
    // Asked on a thread of the mount, before the job is queued.
    let on_mount_thread = Arc::clone(&operations.operations);
    let fill = operations.fixture.run_on_mount_like_thread(move || (on_mount_thread.find_file_to_fill(id, None), on_mount_thread.find_file_to_fill(id, Some(3)), on_mount_thread.find_file_to_fill(id, Some(0)), on_mount_thread.find_file_to_fill(LocalItemId::new(999), None)));
    let download = FileToFill::Download(ItemToDownload { item: id, onedrive_item: ItemFixture::onedrive_reference_of("remote.txt"), content: ItemFixture::onedrive_content_of(b"in OneDrive") });
    assert_eq!(fill, (Ok(download.clone()), Ok(download), Ok(FileToFill::Nothing), Err(Errno::ESTALE)));
    // An open whose download did not land writes nothing and makes nothing.
    assert_eq!(open(&operations, id).map(|opened| opened.fh), Err(Errno::ESTALE));
    assert_eq!(operations.run(move |operations| operations.truncate(id, None, 3)), Err(Errno::ESTALE));
    assert_eq!(operations.fixture.find_item(id), Some(remote));
    assert!(!operations.fixture.disk_path("remote.txt").exists() && !operations.fixture.item_handles.is_open_for_writing(id));
    assert_eq!(operations.list_records(), []);
    // Once it landed, nothing is to be filled and the open goes through.
    operations.fixture.land_copy(id, b"in OneDrive");
    let on_mount_thread = Arc::clone(&operations.operations);
    assert_eq!(operations.fixture.run_on_mount_like_thread(move || on_mount_thread.find_file_to_fill(id, None)), Ok(FileToFill::Nothing));
    assert!(open(&operations, id).is_ok());
}

#[test]
fn an_empty_file_not_on_disk_is_opened_and_said_on_disk_in_one_transaction() {
    let operations = OperationsFixture::new();
    operations.fixture.show_onedrive_directory("docs");
    let empty = operations.fixture.show_onedrive_file("docs/empty.txt", b"").local_item_id;
    // When the row is written the file is not made yet: the row says both at once, and
    // the change is on record.
    let (store, on_disk) = (operations.fixture.store.clone(), operations.fixture.disk_path("docs/empty.txt"));
    operations.run_at(OperationPoint::RowWritten, move || {
        let (row, records) = store.call_blocking(move |store| Ok((store.item_store().reader().find_item(empty)?.unwrap(), store.item_store().list_place_change_records()?))).unwrap();
        assert_eq!((row.content_presence, row.content_before_write.is_some(), on_disk.exists()), (ContentPresence::OnDisk, true, false));
        assert!(matches!(&records[..], [(_, PlaceChangeRecord::MakeEntry { kind: ItemKind::File, owner: MadeEntryOwner::Item(item), .. })] if *item == empty), "{records:?}");
    });
    let opened = open(&operations, empty).unwrap();
    assert_eq!(operations.fixture.read_disk_file("docs/empty.txt").as_deref(), Some(&b""[..]));
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, Some(onedrive_id_of("docs")), "the directory above was made, marked");
    assert_eq!(operations.list_records(), [], "the record is taken once the file is made");
    assert!(operations.fixture.item_handles.is_open_for_writing(empty));
    operations.write_through(opened.fh, b"written");
    operations.close(opened.fh);
    assert_eq!(content_of(&operations, empty).size(), 7);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn an_empty_file_opened_and_closed_untouched_is_a_clean_copy_again() {
    let operations = OperationsFixture::new();
    let empty = operations.fixture.show_onedrive_file("empty.txt", b"");
    let opened = open(&operations, empty.local_item_id).unwrap();
    operations.close(opened.fh);
    let row = operations.fixture.find_item(empty.local_item_id).unwrap();
    assert_eq!((row.local.clone(), row.content_before_write.clone(), row.content_presence, row.file_sync_state()), (empty.local.clone(), None, ContentPresence::OnDisk, Some(FileSyncState::Synced)));
    // The made file is the whole copy of that content: its marks and its time say so.
    let ContentVersion::Hashed { ctag, .. } = ItemFixture::onedrive_content_of(b"") else { unreachable!() };
    assert_eq!(operations.fixture.read_disk_marks("empty.txt"), DiskMarksRead { onedrive_item_id: Some(onedrive_id_of("empty.txt")), is_whole: true, ctag });
    assert_eq!(disk_time(&operations, "empty.txt"), (ONEDRIVE_MTIME, 0));
    operations.fixture.assert_rows_and_disk_agree();
    // So the check at start finds nothing to change.
    let rows = operations.fixture.list_items();
    operations.fixture.run_start_check();
    assert_eq!(operations.fixture.list_items(), rows);
}

#[test]
fn a_reader_between_the_row_that_says_on_disk_and_the_file_being_made_waits_and_gets_the_file() {
    let operations = OperationsFixture::new();
    let empty = operations.fixture.show_onedrive_file("empty.txt", b"").local_item_id;
    // A reader that begins when the row says on disk and no file is there: it sleeps for
    // the place change, and opens the file that was made; it is never told "no file".
    let reader: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<bool>>>> = Arc::default();
    let (place_changes, read_connections, started) = (Arc::clone(&operations.fixture.place_changes), Arc::clone(&operations.fixture.read_connections), Arc::clone(&reader));
    operations.run_at(OperationPoint::RowWritten, move || {
        let (place_changes, read_connections) = (Arc::clone(&place_changes), Arc::clone(&read_connections));
        let thread = std::thread::spawn(move || {
            let opened = place_changes.open_at_row_path(OpenMode::Read, || read_connections.read(|reader| Ok(reader.find_path(empty)?.map(|path| (path, ())))));
            matches!(opened, Ok(Some(_)))
        });
        *crate::panic::lock(&started) = Some(thread);
        std::thread::sleep(Duration::from_millis(30));
    });
    open(&operations, empty).unwrap();
    assert!(crate::panic::lock(&reader).take().unwrap().join().unwrap(), "the reader got the file");
}

#[test]
fn a_file_opened_to_be_overwritten_is_not_downloaded() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    let opened = overwrite(&operations, remote).unwrap();
    assert_eq!(operations.fixture.fixture_content_source().list_fetches(), [], "nothing was fetched");
    assert_eq!(operations.fixture.read_disk_file("remote.txt").as_deref(), Some(&b""[..]));
    let row = operations.fixture.find_item(remote).unwrap();
    assert_eq!((row.content_presence, row.content_before_write.clone()), (ContentPresence::OnDisk, Some(ItemFixture::onedrive_content_of(b"in OneDrive"))));
    // The file is the user's own from its making: the item's, and no whole copy of what OneDrive has.
    assert_eq!(operations.fixture.read_disk_marks("remote.txt"), DiskMarksRead { onedrive_item_id: Some(onedrive_id_of("remote.txt")), is_whole: false, ctag: None });
    assert_eq!(operations.list_records(), []);
    // Closed with nothing more written, it is changed all the same: it was cut to nothing.
    operations.close(opened.fh);
    let row = operations.fixture.find_item(remote).unwrap();
    assert!(matches!(row.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 0, .. })), "{row:?}");
    assert_eq!((row.content_before_write.clone(), row.file_sync_state()), (None, Some(FileSyncState::ModifiedHere)));
    operations.fixture.assert_rows_and_disk_agree();
    // One that is on disk is cut where it is.
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = overwrite(&operations, copy).unwrap();
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b""[..]));
    operations.close(opened.fh);
    assert_eq!(content_of(&operations, copy).size(), 0);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn the_last_close_of_a_changed_file_writes_the_size_and_times_of_the_backing_file() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = open(&operations, copy).unwrap();
    operations.write_through(opened.fh, b"changed, and longer");
    operations.fixture.set_disk_mtime("doc.txt", 1_650_000_000);
    operations.close(opened.fh);
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!(row.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 19, mtime: 1_650_000_000, mtime_nsec: 0, change_count: 1 }));
    assert_eq!((row.content_before_write.clone(), row.file_sync_state()), (None, Some(FileSyncState::ModifiedHere)));
    assert!(row.unsent_since.is_some());
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn the_last_close_of_an_untouched_file_puts_the_content_before_the_write_back() {
    let operations = OperationsFixture::new();
    let copy = operations.fixture.show_onedrive_file("doc.txt", b"the copy");
    operations.fixture.land_copy(copy.local_item_id, b"the copy");
    let clean = operations.fixture.find_item(copy.local_item_id).unwrap();
    let opened = open(&operations, copy.local_item_id).unwrap();
    assert_ne!(operations.fixture.find_item(copy.local_item_id).unwrap(), clean);
    operations.close(opened.fh);
    assert_eq!(operations.fixture.find_item(copy.local_item_id).unwrap(), clean, "the row is as before the open: nothing is unsent");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_close_while_another_writer_holds_the_file_writes_nothing() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let (first, second) = (open(&operations, copy).unwrap(), open(&operations, copy).unwrap());
    operations.write_through(first.fh, b"changed");
    let row = operations.fixture.find_item(copy).unwrap();
    operations.take_wake();
    operations.close(first.fh);
    assert_eq!(operations.fixture.find_item(copy).unwrap(), row);
    assert!(operations.fixture.item_handles.is_open_for_writing(copy) && !operations.take_wake());
    // The last one writes what the first wrote.
    operations.close(second.fh);
    assert_eq!((content_of(&operations, copy).size(), operations.fixture.item_handles.is_open_for_writing(copy)), (8, false));
    // An fh that is no writer's is nothing.
    operations.close(second.fh);
    operations.close(Fh(999));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn the_writer_leaves_the_handles_only_after_the_row_of_its_last_close_is_written() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = open(&operations, copy).unwrap();
    operations.write_through(opened.fh, b"changed, and longer");
    // The store's thread is held up by a job that looks, from the other side, at what a
    // getattr would find while the close waits for its row: the writer's file.
    let (handles, seen) = (Arc::clone(&operations.fixture.item_handles), Arc::new(std::sync::Mutex::new(None)));
    let (store, seen_by_job) = (operations.fixture.store.clone(), Arc::clone(&seen));
    let fh = opened.fh;
    operations.run(move |operations| {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (begin, begun) = std::sync::mpsc::channel::<()>();
        let blocker = std::thread::spawn(move || {
            store.call_blocking(move |_| {
                drop(begin);
                let _ = released.recv();
                Ok(())
            })
        });
        // The store's thread runs that job now: nothing is written until it is let go.
        let _ = begun.recv();
        // The close queues its row behind that job; while it waits, the writer is found.
        let watcher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            *crate::panic::lock(&seen_by_job) = Some(handles.find_writer_file(copy).is_some());
            drop(release);
        });
        operations.close_writer(fh);
        watcher.join().unwrap();
        blocker.join().unwrap().unwrap();
    });
    assert_eq!(*crate::panic::lock(&seen), Some(true), "the writer is among the handles until its row is written");
    assert!(operations.fixture.item_handles.find_writer_file(copy).is_none());
    assert_eq!(content_of(&operations, copy).size(), 19);
}

#[test]
fn a_truncation_with_no_writer_writes_the_new_size_before_the_file_is_cut() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    assert_eq!(operations.run(move |operations| operations.truncate(copy, None, 3)), Ok(()));
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"the"[..]));
    let row = operations.fixture.find_item(copy).unwrap();
    assert!(matches!(row.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 3, change_count: 1, .. })), "{row:?}");
    assert_eq!((row.content_before_write.clone(), row.file_sync_state()), (None, Some(FileSyncState::ModifiedHere)), "no writer's close is to follow");
    assert!(!operations.fixture.item_handles.is_open_for_writing(copy));
    // Grown, too.
    assert_eq!(operations.run(move |operations| operations.truncate(copy, None, 6)), Ok(()));
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"the\0\0\0"[..]));
    assert!(matches!(content_of(&operations, copy), ContentVersion::ChangedHere { size: 6, change_count: 2, .. }));
    operations.fixture.assert_rows_and_disk_agree();
    // A file that is not on disk, cut to nothing: on disk in the same transaction, then made.
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    let (store, on_disk) = (operations.fixture.store.clone(), operations.fixture.disk_path("remote.txt"));
    operations.run_at(OperationPoint::RowWritten, move || {
        let row = store.call_blocking(move |store| store.item_store().reader().find_item(remote)).unwrap().unwrap();
        assert_eq!((row.content_presence, row.local.unwrap().content.unwrap().size(), on_disk.exists()), (ContentPresence::OnDisk, 0, false), "the row before the file");
    });
    assert_eq!(operations.run(move |operations| operations.truncate(remote, None, 0)), Ok(()));
    assert_eq!(operations.fixture.read_disk_file("remote.txt").as_deref(), Some(&b""[..]));
    assert_eq!(operations.fixture.fixture_content_source().list_fetches(), []);
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
    // What is no file the folder shows.
    operations.fixture.make_own_directory("d");
    let directory = operations.id("d");
    assert_eq!(operations.run(move |operations| operations.truncate(directory, None, 0)), Err(Errno::EISDIR));
    assert_eq!(operations.run(|operations| operations.truncate(LocalItemId::new(999), None, 0)), Err(Errno::ESTALE));
}

#[test]
fn a_truncated_file_has_the_time_its_row_was_given() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    operations.run(move |operations| operations.truncate(copy, None, 3)).unwrap();
    operations.run(move |operations| operations.truncate(remote, None, 0)).unwrap();
    for (item, path) in [(copy, "doc.txt"), (remote, "remote.txt")] {
        assert_eq!(content_of(&operations, item).mtime(), disk_time(&operations, path), "{path}: a step finds the content it was planned from by its size and time");
        assert_ne!(disk_time(&operations, path).0, ONEDRIVE_MTIME);
    }
}

#[test]
fn a_truncation_of_a_file_open_for_writing_marks_it_written_and_writes_no_row() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = open(&operations, copy).unwrap();
    let row = operations.fixture.find_item(copy).unwrap();
    // By the item alone, with no fh given.
    assert_eq!(operations.run(move |operations| operations.truncate(copy, None, 3)), Ok(()));
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"the"[..]));
    assert_eq!(operations.fixture.find_item(copy).unwrap(), row, "no row: the last close writes the size");
    operations.close(opened.fh);
    assert!(matches!(content_of(&operations, copy), ContentVersion::ChangedHere { size: 3, .. }), "the close says the content changed");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_truncation_through_a_handle_whose_row_is_gone_cuts_the_handles_file() {
    let operations = OperationsFixture::new();
    let made = operations.make("open.txt", MadeKind::FileOpenedForWriting).unwrap();
    let id = made.item.local_item_id;
    operations.write_through(made.fh, b"written");
    operations.remove("open.txt", ItemKind::File).unwrap();
    assert_eq!(operations.fixture.find_item(id), None);
    let rows = operations.fixture.list_items();
    let fh = made.fh;
    assert_eq!(operations.run(move |operations| operations.truncate(id, Some(fh), 3)), Ok(()));
    assert_eq!(operations.run(move |operations| operations.set_modified_time(id, Some(fh), 1_650_000_000, 0)), Ok(()));
    let (_, backing_file) = operations.fixture.item_handles.find_file_to_write(fh).unwrap();
    assert_eq!((backing_file.metadata().unwrap().len(), backing_file.metadata().unwrap().mtime()), (3, 1_650_000_000));
    assert_eq!(operations.fixture.list_items(), rows, "no row is written");
    // Without the fh there is nothing to answer from.
    operations.close(fh);
    assert_eq!(operations.run(move |operations| operations.truncate(id, None, 3)), Err(Errno::ESTALE));
    assert_eq!(operations.run(move |operations| operations.set_modified_time(id, None, 1, 0)), Err(Errno::ESTALE));
}

#[test]
fn a_time_set_alone_goes_to_the_backing_file_and_to_the_row() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    assert_eq!(operations.run(move |operations| operations.set_modified_time(copy, None, 1_650_000_000, 123)), Ok(()));
    assert_eq!(disk_time(&operations, "doc.txt"), (1_650_000_000, 123));
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!(row.local.as_ref().unwrap().content.as_ref().unwrap().mtime(), (1_650_000_000, 123));
    assert_eq!((row.file_sync_state(), row.unsent_since), (Some(FileSyncState::Synced), None), "a clean copy stays clean: nothing is sent");
    // A file that is not on disk: the row alone.
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    assert_eq!(operations.run(move |operations| operations.set_modified_time(remote, None, 1_650_000_000, 0)), Ok(()));
    assert_eq!(content_of(&operations, remote).mtime(), (1_650_000_000, 0));
    assert!(!operations.fixture.disk_path("remote.txt").exists());
    // A file of the user's own that is changed: the time of that content.
    let mine = operations.fixture.make_own_file("mine.txt", b"mine").local_item_id;
    assert_eq!(operations.run(move |operations| operations.set_modified_time(mine, None, 1_650_000_000, 0)), Ok(()));
    assert_eq!((content_of(&operations, mine).mtime(), disk_time(&operations, "mine.txt")), ((1_650_000_000, 0), (1_650_000_000, 0)));
    // A directory: its row keeps no time; its directory on disk gets it.
    let directory = operations.fixture.make_own_directory("d").local_item_id;
    let rows = operations.fixture.list_items();
    assert_eq!(operations.run(move |operations| operations.set_modified_time(directory, None, 1_650_000_000, 0)), Ok(()));
    assert_eq!((disk_time(&operations, "d"), operations.fixture.list_items()), ((1_650_000_000, 0), rows));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_time_set_through_a_writer_is_written_by_its_close() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = open(&operations, copy).unwrap();
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!(operations.run(move |operations| operations.set_modified_time(copy, None, 1_650_000_000, 0)), Ok(()));
    assert_eq!(disk_time(&operations, "doc.txt"), (1_650_000_000, 0));
    assert_eq!(operations.fixture.find_item(copy).unwrap(), row, "the backing file alone");
    operations.close(opened.fh);
    assert_eq!(content_of(&operations, copy), ContentVersion::ChangedHere { size: 8, mtime: 1_650_000_000, mtime_nsec: 0, change_count: 1 }, "the row and the file say one time");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_close_in_a_folder_switched_to_read_only_meanwhile_is_still_written() {
    let operations = OperationsFixture::new();
    let copy = land(&operations, "doc.txt", b"the copy");
    let opened = open(&operations, copy).unwrap();
    operations.write_through(opened.fh, b"changed, and longer");
    operations.fixture.access_mode_switch.switch_to_read_only();
    operations.close(opened.fh);
    let row = operations.fixture.find_item(copy).unwrap();
    assert_eq!((row.local.as_ref().unwrap().content.as_ref().unwrap().size(), row.content_before_write.clone()), (19, None));
    assert!(!operations.fixture.item_handles.is_open_for_writing(copy));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_backing_file_that_cannot_be_made_after_its_row_asks_for_the_take_down() {
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    // The disk holds something at the name that no row says: the file cannot be made.
    operations.fixture.write_disk_file("remote.txt", b"what no row says");
    assert_eq!(overwrite(&operations, remote).map(|opened| opened.fh), Err(Errno::EIO));
    assert_eq!(operations.take_downs(), [crate::mount::mount_take_down::TakeDownCause::BackingFileBehindRow { path: "remote.txt".into() }]);
    assert!(!operations.fixture.access_mode_switch.is_read_write());
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::MakeEntry { owner: MadeEntryOwner::Item(item), .. }] if item == remote), "the record stands");
    assert_eq!(operations.fixture.read_disk_file("remote.txt").as_deref(), Some(&b"what no row says"[..]), "nothing was replaced");
    assert!(!operations.fixture.item_handles.is_open_for_writing(remote));
    // The check at start settles it: the rows and the disk agree again.
    operations.fixture.run_start_check();
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

/// The review's M1: what a restart of the daemon leaves of a program that holds a file
/// open, wrote into it and put its time back (`cp -p` does, before its close).
#[test]
fn bytes_written_through_a_handle_that_was_never_closed_are_unsent_after_the_check_at_start_though_size_and_time_stand() {
    let operations = OperationsFixture::new();
    let doc = land(&operations, "doc.txt", b"as it was");
    let clean = operations.fixture.find_item(doc).unwrap();
    let fh = open(&operations, doc).unwrap().fh;
    operations.write_through(fh, b"AS IT WAS");
    operations.run(move |operations| operations.set_modified_time(doc, Some(fh), ONEDRIVE_MTIME, 0)).unwrap();
    assert_eq!(disk_time(&operations, "doc.txt"), (ONEDRIVE_MTIME, 0), "the file has the size and the time of the clean copy");
    // No close: the daemon stops, or dies.
    operations.fixture.run_start_check();
    let row = operations.fixture.find_item(doc).unwrap();
    let content = row.local.clone().unwrap().content.unwrap();
    assert!(matches!(content, ContentVersion::ChangedHere { size: 9, mtime: ONEDRIVE_MTIME, .. }), "the row says the content was changed here: {content:?}");
    assert_eq!(row.content_before_write, None, "as a close that changed the file leaves it");
    assert_eq!((row.synced_content.clone(), row.content_presence), (clean.synced_content.clone(), ContentPresence::OnDisk));
    assert!(row.unsent_since.is_some() && row.file_sync_state() != Some(FileSyncState::Synced), "the row is unsent: {row:?}");
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b"AS IT WAS"[..]), "the bytes to be sent");
    // The check is done with it.
    operations.fixture.run_start_check();
    assert_eq!(operations.fixture.find_item(doc), Some(row));
}

/// What M1's rule costs, accepted by the owner: a file a program held open for writing
/// and never wrote into is sent once more.
#[test]
fn a_file_opened_for_writing_and_never_closed_is_changed_here_after_the_check_at_start_also_when_nothing_was_written() {
    let operations = OperationsFixture::new();
    let doc = land(&operations, "doc.txt", b"as it was");
    open(&operations, doc).unwrap();
    operations.fixture.run_start_check();
    let row = operations.fixture.find_item(doc).unwrap();
    let content = row.local.clone().unwrap().content.unwrap();
    assert!(matches!(content, ContentVersion::ChangedHere { size: 9, mtime: ONEDRIVE_MTIME, .. }), "{content:?}");
    assert_eq!(row.content_before_write, None);
    assert!(row.unsent_since.is_some(), "{row:?}");
}

/// The review's S4: the plan's reads and the bus share the store's one reading thread,
/// and an open for writing does not wait behind them.
#[test]
fn what_is_downloaded_before_a_write_is_read_without_waiting_for_the_stores_reading_thread() {
    use std::sync::mpsc;
    let operations = OperationsFixture::new();
    let remote = operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive").local_item_id;
    // The store's reading thread is taken, as by the plan's read of every row.
    let ((let_go, held), (taken_sender, taken)) = (mpsc::channel::<()>(), mpsc::channel::<()>());
    let store = operations.fixture.store.clone();
    let holder = std::thread::spawn(move || {
        store.read_items_blocking(move |_| {
            taken_sender.send(()).unwrap();
            let _ = held.recv();
            Ok(())
        })
    });
    taken.recv().unwrap();
    let (answer_sender, answer) = mpsc::channel();
    let on_mount_thread = Arc::clone(&operations.operations);
    let asker = std::thread::spawn(move || answer_sender.send(on_mount_thread.find_file_to_fill(remote, None)));
    let answered = answer.recv_timeout(Duration::from_secs(2));
    let_go.send(()).unwrap();
    holder.join().unwrap().unwrap();
    asker.join().unwrap().unwrap();
    let answered = answered.expect("answered while the store's reading thread is taken");
    assert!(matches!(answered, Ok(FileToFill::Download(ItemToDownload { item, .. })) if item == remote), "{answered:?}");
}
