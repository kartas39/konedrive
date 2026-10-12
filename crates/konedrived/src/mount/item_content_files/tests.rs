use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_tree::{ConfirmedItem, ContentVersion, ItemPlace, ItemVersions, LocalItemId, LocalVersionChange, StepCommitOutcome};

use super::*;
use crate::fake_onedrive::qx;
use crate::sync_parts::step_outcome::{StepObstacle, StepTrouble};
use crate::mount::item_fixture::{DiskMarksRead, ItemFixture, ONEDRIVE_MTIME, START_TIME};

fn make_content_files(fixture: &ItemFixture) -> Arc<ItemContentFiles> {
    Arc::new(ItemContentFiles::new(Arc::clone(&fixture.place_changes), fixture.store_path.clone(), Arc::clone(&fixture.item_handles)))
}

/// Opens the local content of `item` as a step does: on a thread like the mount's.
fn open_to_send(fixture: &ItemFixture, content_files: &Arc<ItemContentFiles>, item: &ItemVersions) -> Result<LocalContentFile, StepTrouble> {
    let (content_files, item) = (Arc::clone(content_files), item.clone());
    fixture.run_on_mount_like_thread(move || content_files.open_to_send(&item))
}

/// Runs `mark` over the content files on a thread like the mount's.
fn mark<T: Send + 'static>(fixture: &ItemFixture, content_files: &Arc<ItemContentFiles>, mark: impl FnOnce(&ItemContentFiles) -> T + Send + 'static) -> T {
    let content_files = Arc::clone(content_files);
    fixture.run_on_mount_like_thread(move || mark(&content_files))
}

fn confirmed_file(path: &str, content: &[u8], ctag: &str, mtime: i64) -> ConfirmedItem {
    ConfirmedItem {
        item: ItemFixture::onedrive_reference_of(path),
        parent: ItemFixture::onedrive_reference_of(""),
        name: path.into(),
        content: Some(ContentVersion::Hashed { size: content.len() as u64, mtime, mtime_nsec: 0, hash: Some(qx(content)), ctag: Some(ctag.to_owned()) }),
        etag: "e-sent".into(),
    }
}

fn disk_mtime(fixture: &ItemFixture, path: &str) -> i64 {
    fs::metadata(fixture.disk_path(path)).expect("the file").modified().expect("its time").duration_since(UNIX_EPOCH).expect("after 1970").as_secs() as i64
}

fn unmarked() -> DiskMarksRead {
    DiskMarksRead { onedrive_item_id: None, is_whole: false, ctag: None }
}

#[test]
fn a_file_has_the_size_and_time_of_a_content_by_its_size_its_seconds_and_the_nanoseconds_the_content_has() {
    let whole_seconds = ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: None, ctag: None };
    let with_nanoseconds = ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 7, change_count: 1 };
    assert!(has_size_and_time_of(&whole_seconds, 3, 100, 0));
    assert!(has_size_and_time_of(&whole_seconds, 3, 100, 999), "a content told in whole seconds asks no nanoseconds");
    assert!(!has_size_and_time_of(&whole_seconds, 4, 100, 0), "another size");
    assert!(!has_size_and_time_of(&whole_seconds, 3, 101, 0), "another second");
    assert!(has_size_and_time_of(&with_nanoseconds, 3, 100, 7));
    assert!(!has_size_and_time_of(&with_nanoseconds, 3, 100, 8), "another nanosecond of a content that has them");
}

#[test]
fn the_content_is_opened_at_the_path_of_the_local_version() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let made = fixture.make_own_file("docs/report.txt", b"the report");
    let content_files = make_content_files(&fixture);

    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");

    assert_eq!(opened.size(), 10);
    assert_eq!(opened.read_fragment(0, 10).expect("the whole content"), b"the report");
    assert_eq!(opened.read_fragment(4, 6).expect("a fragment"), b"report");
}

#[test]
fn a_content_the_row_no_longer_has_is_not_opened() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let planned = ItemVersions::of(&made);
    fixture.change_file_here(made.local_item_id, b"another report");
    let content_files = make_content_files(&fixture);

    let opened = open_to_send(&fixture, &content_files, &planned);

    assert_eq!(opened.err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)));
}

#[test]
fn a_row_that_says_on_disk_over_no_file_is_a_missing_backing_file() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    fs::remove_file(fixture.disk_path("report.txt")).expect("the file is removed by hand");
    let content_files = make_content_files(&fixture);

    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made));

    assert_eq!(opened.err(), Some(StepTrouble::Obstacle(StepObstacle::BackingFileMissing)));
}

/// Moves `item` to `name` in the root as a rename of the mount does: the disk through a
/// place change, then the row.
fn rename_in_root(fixture: &ItemFixture, item: LocalItemId, from: &'static str, to: &'static str) {
    let (place_changes, store) = (Arc::clone(&fixture.place_changes), fixture.store.clone());
    fixture.run_on_write_operation_thread(move || {
        let change = place_changes.begin();
        change.move_entry(from.as_ref(), to.as_ref(), &Vec::new()).expect("the entry is moved on disk");
        let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: to.into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored: false })).expect("the row is moved");
        drop(change);
    });
}

#[test]
fn a_file_renamed_twice_between_the_row_and_the_open_is_still_the_rows_file() {
    let fixture = ItemFixture::new();
    let (first, second) = (fixture.make_own_file("x", b"the first file"), fixture.make_own_file("y", b"the other file"));
    let (first_id, second_id) = (first.local_item_id, second.local_item_id);
    let content_files = make_content_files(&fixture);
    let planned = ItemVersions::of(&first);
    let is_exchanging = AtomicBool::new(true);

    std::thread::scope(|scope| {
        // The two files exchange their names, again and again: each exchange is three
        // renames, after which the first file is where the other was.
        scope.spawn(|| {
            for round in 0..60 {
                let (first_at, second_at) = if round % 2 == 0 { ("x", "y") } else { ("y", "x") };
                rename_in_root(&fixture, first_id, first_at, "between");
                rename_in_root(&fixture, second_id, second_at, first_at);
                rename_in_root(&fixture, first_id, "between", second_at);
            }
            is_exchanging.store(false, Ordering::SeqCst);
        });
        let mut opened_count = 0;
        while is_exchanging.load(Ordering::SeqCst) || opened_count == 0 {
            let opened = open_to_send(&fixture, &content_files, &planned).expect("the first file is opened, wherever it is");
            assert_eq!(opened.read_fragment(0, 14).expect("its content"), b"the first file", "the file opened is the row's, never the other item's");
            opened_count += 1;
        }
    });
}

#[test]
fn a_file_whose_size_or_time_is_not_the_planned_contents_is_not_opened() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let planned = ItemVersions::of(&made);

    fixture.set_disk_mtime("report.txt", START_TIME + 5);
    assert_eq!(open_to_send(&fixture, &content_files, &planned).err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)), "another time");

    fs::write(fixture.disk_path("report.txt"), b"a longer report").expect("the file is written by hand");
    fixture.set_disk_mtime("report.txt", START_TIME);
    assert_eq!(open_to_send(&fixture, &content_files, &planned).err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)), "another size");

    fs::write(fixture.disk_path("report.txt"), b"the report").expect("the file is written by hand");
    fixture.set_disk_mtime("report.txt", START_TIME);
    assert!(open_to_send(&fixture, &content_files, &planned).is_ok(), "the size and the time of the planned content again");
}

#[test]
fn a_file_a_program_holds_open_for_writing_is_not_opened() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let writer_file = Arc::new(fs::File::open(fixture.disk_path("report.txt")).expect("the writer's file"));
    let (writer, _) = fixture.item_handles.add_writer(made.local_item_id, writer_file);

    assert_eq!(open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).err(), Some(StepTrouble::Obstacle(StepObstacle::OpenForWriting)));

    fixture.item_handles.remove_writer(writer);
    assert!(open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).is_ok(), "no writer any more");
}

#[test]
fn a_fragment_read_after_the_file_changed_says_so() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");

    fs::write(fixture.disk_path("report.txt"), b"THE REPORT").expect("the file is written by hand");
    fixture.set_disk_mtime("report.txt", START_TIME + 60);

    assert_eq!(opened.read_fragment(0, 10).err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)), "the same size at another time");
    assert_eq!(opened.hash().err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)));

    fs::write(fixture.disk_path("report.txt"), b"short").expect("the file is written by hand");
    fixture.set_disk_mtime("report.txt", START_TIME);
    assert_eq!(opened.read_fragment(0, 10).err(), Some(StepTrouble::Obstacle(StepObstacle::ContentChanged)), "shorter than its content");
}

#[test]
fn the_hash_of_the_content_is_the_quickxorhash_of_its_bytes() {
    let fixture = ItemFixture::new();
    // More than is hashed at once, and not a whole number of such parts.
    let content: Vec<u8> = (0..HASHED_AT_ONCE * 2 + 4321).map(|byte| (byte % 251) as u8).collect();
    let made = fixture.make_own_file("large.bin", &content);
    let content_files = make_content_files(&fixture);
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");

    assert_eq!(opened.hash().expect("the hash"), qx(&content));

    let empty = fixture.make_own_file("empty.bin", b"");
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&empty)).expect("an empty content is opened");
    assert_eq!(opened.hash().expect("the hash of nothing"), qx(b""));
}

#[test]
fn a_sent_file_is_marked_with_the_item_id_the_whole_copy_state_and_the_ctag() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");
    let sent = confirmed_file("report.txt", b"the report", "c-sent", ONEDRIVE_MTIME);

    content_files.mark_sent_file(&opened, &sent).expect("the sent file is marked");

    assert_eq!(fixture.read_disk_marks("report.txt"), DiskMarksRead { onedrive_item_id: Some("report.txt".into()), is_whole: true, ctag: Some("c-sent".into()) });
    assert_eq!(disk_mtime(&fixture, "report.txt"), ONEDRIVE_MTIME, "the file has the time of the content the row has from now on");
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
}

#[test]
fn a_sent_file_that_changed_since_is_not_marked() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");
    fixture.change_file_here(made.local_item_id, b"another report");

    content_files.mark_sent_file(&opened, &confirmed_file("report.txt", b"the report", "c-sent", ONEDRIVE_MTIME)).expect("nothing to mark is no failure");

    assert_eq!(fixture.read_disk_marks("report.txt"), unmarked(), "the file holds another content than was sent");
    assert_eq!(disk_mtime(&fixture, "report.txt"), START_TIME);
}

#[test]
fn a_file_freed_or_renamed_during_its_upload_is_marked_through_its_descriptor() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_file("report.txt", b"the report");
    let content_files = make_content_files(&fixture);
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&made)).expect("the local content is opened");
    rename_in_root(&fixture, made.local_item_id, "report.txt", "moved.txt");

    content_files.mark_sent_file(&opened, &confirmed_file("report.txt", b"the report", "c-sent", ONEDRIVE_MTIME)).expect("the sent file is marked");

    assert_eq!(fixture.read_disk_marks("moved.txt"), DiskMarksRead { onedrive_item_id: Some("report.txt".into()), is_whole: true, ctag: Some("c-sent".into()) }, "marked where it is by now");

    // Removed from its directory: the marks go to the file the descriptor holds, and nowhere else.
    let other = fixture.make_own_file("other.txt", b"the other");
    let opened = open_to_send(&fixture, &content_files, &ItemVersions::of(&other)).expect("the local content is opened");
    fs::remove_file(fixture.disk_path("other.txt")).expect("the file is removed");
    fixture.write_disk_file("other.txt", b"a new file");
    content_files.mark_sent_file(&opened, &confirmed_file("other.txt", b"the other", "c-other", ONEDRIVE_MTIME)).expect("a file that has no name is marked all the same");
    assert_eq!(fixture.read_disk_marks("other.txt"), unmarked(), "what has the name now is another file");
}

#[test]
fn a_directory_made_in_onedrive_is_marked_with_its_item_id_when_it_is_on_disk() {
    let fixture = ItemFixture::new();
    let made = fixture.make_own_directory("photos");
    let content_files = make_content_files(&fixture);
    let (versions, confirmed) = (ItemVersions::of(&made), ConfirmedItem { item: ItemFixture::onedrive_reference_of("photos"), parent: ItemFixture::onedrive_reference_of(""), name: "photos".into(), content: None, etag: "e-photos".into() });

    let (step, answer) = (versions.clone(), confirmed.clone());
    mark(&fixture, &content_files, move |content_files| content_files.mark_made_directory(&step, &answer)).expect("a directory with no binding is left");
    assert_eq!(fixture.read_disk_marks("photos"), unmarked(), "the row has not the binding OneDrive answered: its commit was not written");

    let (step, answer) = (versions.clone(), confirmed.clone());
    let committed = fixture.write_rows(move |store| store.item_store().commit_make_directory(&step, &answer.parent, "photos".as_ref(), &answer));
    assert_eq!(committed, StepCommitOutcome::Written);
    let (step, answer) = (versions.clone(), confirmed.clone());
    mark(&fixture, &content_files, move |content_files| content_files.mark_made_directory(&step, &answer)).expect("the made directory is marked");
    assert_eq!(fixture.read_disk_marks("photos"), DiskMarksRead { onedrive_item_id: Some("photos".into()), is_whole: false, ctag: None });

    // One that is not on disk has nothing to mark.
    fs::remove_dir(fixture.disk_path("photos")).expect("the directory is removed by hand");
    mark(&fixture, &content_files, move |content_files| content_files.mark_made_directory(&versions, &confirmed)).expect("a directory that is not on disk is no failure");
}

#[test]
fn what_the_two_sides_agree_on_is_marked_as_the_whole_copy_with_the_bindings_id_and_the_ctag() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("docs/report.txt", b"the report");
    fixture.land_copy(file.local_item_id, b"the report");
    fixture.write_disk_marks("docs", &unmarked());
    fixture.write_disk_marks("docs/report.txt", &unmarked());
    fixture.set_disk_mtime("docs/report.txt", START_TIME);
    let content_files = make_content_files(&fixture);

    let (file_id, directory_id) = (file.local_item_id, directory.local_item_id);
    mark(&fixture, &content_files, move |content_files| content_files.mark_agreed_content(file_id)).expect("the file is marked");
    mark(&fixture, &content_files, move |content_files| content_files.mark_agreed_content(directory_id)).expect("the directory is marked");

    let ctag = format!("c-{}", qx(b"the report"));
    assert_eq!(fixture.read_disk_marks("docs/report.txt"), DiskMarksRead { onedrive_item_id: Some("docs!report.txt".into()), is_whole: true, ctag: Some(ctag) });
    assert_eq!(disk_mtime(&fixture, "docs/report.txt"), ONEDRIVE_MTIME, "the time of the content agreed on");
    assert_eq!(fixture.read_disk_marks("docs"), DiskMarksRead { onedrive_item_id: Some("docs".into()), is_whole: false, ctag: None });

    // A file changed here since is the entry of its item of OneDrive and no whole copy.
    fixture.change_file_here(file_id, b"changed here");
    fixture.write_disk_marks("docs/report.txt", &unmarked());
    mark(&fixture, &content_files, move |content_files| content_files.mark_agreed_content(file_id)).expect("the changed file is marked");
    assert_eq!(fixture.read_disk_marks("docs/report.txt"), DiskMarksRead { onedrive_item_id: Some("docs!report.txt".into()), is_whole: false, ctag: None });
    assert_eq!(disk_mtime(&fixture, "docs/report.txt"), START_TIME, "its time is its own");
}

#[test]
fn an_item_that_gave_its_binding_up_loses_its_marks() {
    let fixture = ItemFixture::new();
    let own = fixture.make_own_file("report.txt", b"the report");
    fixture.write_disk_marks("report.txt", &DiskMarksRead { onedrive_item_id: Some("gone".into()), is_whole: true, ctag: Some("c-gone".into()) });
    xattr::set(fixture.disk_path("report.txt"), konedrive_fs::placeholder::XATTR_PIN, b"1").expect("another attribute of konedrive's");
    let bound = fixture.show_onedrive_file("kept.txt", b"kept");
    fixture.land_copy(bound.local_item_id, b"kept");
    let marks_of_bound = fixture.read_disk_marks("kept.txt");
    let content_files = make_content_files(&fixture);

    let (own_id, bound_id) = (own.local_item_id, bound.local_item_id);
    mark(&fixture, &content_files, move |content_files| content_files.remove_onedrive_marks(own_id)).expect("the marks are taken off");
    mark(&fixture, &content_files, move |content_files| content_files.remove_onedrive_marks(bound_id)).expect("an item with a binding is left");

    assert_eq!(fixture.read_disk_marks("report.txt"), unmarked());
    assert!(xattr::get(fixture.disk_path("report.txt"), konedrive_fs::placeholder::XATTR_PIN).expect("the attributes are read").is_some(), "what else konedrive keeps on it stays");
    assert_eq!(fixture.read_disk_marks("kept.txt"), marks_of_bound, "an item that has a binding keeps its marks");
}

#[test]
fn the_rows_are_read_without_waiting_for_the_stores_reading_thread() {
    use std::sync::mpsc;
    use std::time::Duration;
    let fixture = ItemFixture::new();
    let own = fixture.make_own_file("own.txt", b"the user's");
    let directory = fixture.make_own_directory("made");
    let content_files = make_content_files(&fixture);
    // The store's reading thread is taken, as by the plan's read of every row.
    let ((let_go, held), (taken_sender, taken)) = (mpsc::channel::<()>(), mpsc::channel::<()>());
    let store = fixture.store.clone();
    let holder = std::thread::spawn(move || {
        store.read_items_blocking(move |_| {
            taken_sender.send(()).unwrap();
            let _ = held.recv();
            Ok(())
        })
    });
    taken.recv().unwrap();

    let (answer_sender, answer) = mpsc::channel();
    let (asked, versions, directory) = (Arc::clone(&content_files), ItemVersions::of(&own), directory.local_item_id);
    let asker = std::thread::spawn(move || {
        let opened = asked.open_to_send(&versions).map(|file| file.size());
        let marked = asked.remove_onedrive_marks(directory).map_err(|error| error.to_string());
        let _ = answer_sender.send((opened, marked));
    });
    let answered = answer.recv_timeout(Duration::from_secs(2));
    let_go.send(()).unwrap();
    holder.join().unwrap().unwrap();
    asker.join().unwrap();

    assert_eq!(answered.map_err(|_| "nothing is answered while the store's reading thread is taken"), Ok((Ok(10), Ok(()))));
}

#[test]
fn the_time_a_file_is_given_is_its_contents_with_the_nanoseconds() {
    use std::time::Duration;
    let content = ContentVersion::ChangedHere { size: 3, mtime: 1_700_000_000, mtime_nsec: 123_456_789, change_count: 1 };
    assert_eq!(to_content_time(&content), UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789), "a file given this time has the size and the time of its content");
    let listed = ContentVersion::Hashed { size: 3, mtime: 1_600_000_000, mtime_nsec: 0, hash: None, ctag: None };
    assert_eq!(to_content_time(&listed), UNIX_EPOCH + Duration::from_secs(1_600_000_000));
}
