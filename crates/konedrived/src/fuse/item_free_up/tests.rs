use std::fs;
use std::os::unix::fs::PermissionsExt;

use konedrive_tree::{ContentPresence, Item, PinChange};

use super::*;
use crate::folder::running_steps::{RunningStep, StepClass};
use crate::fuse::item_download_states::{DownloadAsker, DownloadKey};
use crate::fuse::item_fixture::ItemFixture;

fn make_free_up(fixture: &ItemFixture) -> Arc<ItemFreeUp> {
    Arc::new(ItemFreeUp::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.item_handles), Arc::clone(&fixture.download_states), Arc::clone(&fixture.running_steps)))
}

fn free(fixture: &ItemFixture, free_up: &Arc<ItemFreeUp>, item: &Item) -> FreedCopy {
    let (free_up, item) = (Arc::clone(free_up), item.local_item_id);
    fixture.run_on_write_operation_thread(move || free_up.free_downloaded_copy(item))
}

fn mend(fixture: &ItemFixture, free_up: &Arc<ItemFreeUp>, item: &Item) -> bool {
    let (free_up, item) = (Arc::clone(free_up), item.local_item_id);
    fixture.run_on_write_operation_thread(move || free_up.mend_missing_copy(item))
}

/// A file of OneDrive at `path` with its clean copy on disk.
fn make_clean_copy(fixture: &ItemFixture, path: &str, content: &[u8]) -> Item {
    let shown = fixture.show_onedrive_file(path, content);
    fixture.land_copy(shown.local_item_id, content);
    fixture.find_item(shown.local_item_id).expect("the row")
}

fn pin(fixture: &ItemFixture, item: &Item) {
    let id = item.local_item_id;
    assert_eq!(fixture.write_rows(move |store| store.item_store().set_item_pin(id, true)), PinChange::Written);
}

fn presence_of(fixture: &ItemFixture, row: &Item) -> ContentPresence {
    fixture.find_item(row.local_item_id).expect("the row").content_presence
}

#[test]
fn the_decision_of_a_free_up_is_the_first_reason_that_holds() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let clean = make_clean_copy(&fixture, "clean.txt", b"clean");
    let not_on_disk = fixture.show_onedrive_file("remote.txt", b"remote");
    let own = fixture.make_own_file("own.txt", b"own");
    let changed = fixture.change_file_here(make_clean_copy(&fixture, "changed.txt", b"changed").local_item_id, b"changed here");

    assert_eq!(decide_item_free_up(&clean, false, false, false), FreeUpDecision::Free);
    assert_eq!(decide_item_free_up(&clean, true, true, true), FreeUpDecision::LeavePinned, "a pin before everything");
    assert_eq!(decide_item_free_up(&changed, false, true, true), FreeUpDecision::LeaveModifiedHere, "a change that is not sent before a writer");
    assert_eq!(decide_item_free_up(&own, false, false, false), FreeUpDecision::LeaveModifiedHere, "a file OneDrive does not have: its copy is the only one");
    assert_eq!(decide_item_free_up(&clean, false, true, true), FreeUpDecision::LeaveOpenForWriting, "a writer before a download or a step");
    assert_eq!(decide_item_free_up(&clean, false, true, false), FreeUpDecision::LeaveDownloading);
    assert_eq!(decide_item_free_up(&not_on_disk, false, false, false), FreeUpDecision::NothingToFree);
    assert_eq!(decide_item_free_up(&directory, false, false, false), FreeUpDecision::NothingToFree);
}

#[test]
fn a_clean_copy_is_freed() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let clean = make_clean_copy(&fixture, "docs/report.txt", &[7; 5000]);
    let free_up = make_free_up(&fixture);

    let freed = free(&fixture, &free_up, &clean);

    let FreedCopy::Freed { bytes } = freed else { panic!("the copy is freed: {freed:?}") };
    assert!(bytes >= 4096, "the bytes it took on disk: {bytes}");
    assert_eq!(fixture.read_disk_file("docs/report.txt"), None);
    assert_eq!(presence_of(&fixture, &clean), ContentPresence::NotOnDisk);
    assert_eq!(fs::read_dir(fixture.folder()).unwrap().count(), 1, "nothing is left set aside: the directory alone is in the root");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_pinned_file_is_left() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let (own_pin, pinned_above) = (make_clean_copy(&fixture, "report.txt", b"the report"), make_clean_copy(&fixture, "docs/below.txt", b"below"));
    pin(&fixture, &own_pin);
    pin(&fixture, &directory);
    let free_up = make_free_up(&fixture);

    assert_eq!(free(&fixture, &free_up, &own_pin), FreedCopy::Left(FreeUpDecision::LeavePinned));
    assert_eq!(free(&fixture, &free_up, &pinned_above), FreedCopy::Left(FreeUpDecision::LeavePinned), "the pin of a directory above keeps it");
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
    assert_eq!(presence_of(&fixture, &own_pin), ContentPresence::OnDisk);
}

#[test]
fn a_file_changed_here_is_left_its_copy_is_the_only_one() {
    let fixture = ItemFixture::new();
    let changed = fixture.change_file_here(make_clean_copy(&fixture, "report.txt", b"the report").local_item_id, b"changed here");
    let own = fixture.make_own_file("own.txt", b"own");
    let free_up = make_free_up(&fixture);

    assert_eq!(free(&fixture, &free_up, &changed), FreedCopy::Left(FreeUpDecision::LeaveModifiedHere));
    assert_eq!(free(&fixture, &free_up, &own), FreedCopy::Left(FreeUpDecision::LeaveModifiedHere));
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"changed here"[..]));
    assert_eq!(fixture.read_disk_file("own.txt").as_deref(), Some(&b"own"[..]));
}

#[test]
fn a_file_open_for_writing_is_left() {
    let fixture = ItemFixture::new();
    let clean = make_clean_copy(&fixture, "report.txt", b"the report");
    let free_up = make_free_up(&fixture);
    let (writer, _) = fixture.item_handles.add_writer(clean.local_item_id, Arc::new(fs::File::open(fixture.disk_path("report.txt")).unwrap()));

    assert_eq!(free(&fixture, &free_up, &clean), FreedCopy::Left(FreeUpDecision::LeaveOpenForWriting));
    assert_eq!(presence_of(&fixture, &clean), ContentPresence::OnDisk);

    fixture.item_handles.remove_writer(writer);
    assert!(matches!(free(&fixture, &free_up, &clean), FreedCopy::Freed { .. }));
}

#[test]
fn a_file_being_downloaded_or_sent_is_left() {
    let fixture = ItemFixture::new();
    let (downloading, sent) = (make_clean_copy(&fixture, "downloading.txt", b"downloading"), make_clean_copy(&fixture, "sent.txt", b"sent"));
    let free_up = make_free_up(&fixture);
    // A download of another content of the file runs.
    let key = DownloadKey { item: downloading.local_item_id, content: ItemFixture::onedrive_content_of(b"another content") };
    let _started = fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None);
    assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Content, item: sent.local_item_id, other_items: Vec::new(), destination: None, sent: 0, total: 4 }));

    assert_eq!(free(&fixture, &free_up, &downloading), FreedCopy::Left(FreeUpDecision::LeaveDownloading));
    assert_eq!(free(&fixture, &free_up, &sent), FreedCopy::Left(FreeUpDecision::LeaveDownloading), "a file a step runs for is left as one that is downloaded");
    assert_eq!(fixture.read_disk_file("sent.txt").as_deref(), Some(&b"sent"[..]));
}

#[test]
fn a_file_not_on_disk_and_a_directory_have_nothing_to_free() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    fixture.make_directory_on_disk(directory.local_item_id);
    let not_on_disk = fixture.show_onedrive_file("docs/remote.txt", b"remote");
    pin(&fixture, &not_on_disk);
    let gone = make_clean_copy(&fixture, "gone.txt", b"gone");
    fixture.remove_here(gone.local_item_id);
    let free_up = make_free_up(&fixture);

    assert_eq!(free(&fixture, &free_up, &not_on_disk), FreedCopy::Left(FreeUpDecision::NothingToFree), "what is not on disk takes no space, whatever keeps it");
    assert_eq!(free(&fixture, &free_up, &directory), FreedCopy::Left(FreeUpDecision::NothingToFree));
    assert_eq!(free(&fixture, &free_up, &gone), FreedCopy::Left(FreeUpDecision::NothingToFree), "an item the folder does not show");
    assert!(fixture.disk_path("docs").is_dir());
}

#[test]
fn the_row_says_not_on_disk_before_the_copy_goes() {
    let fixture = ItemFixture::new();
    let clean = make_clean_copy(&fixture, "report.txt", b"the report");
    let free_up = make_free_up(&fixture);
    let (id, folder, store) = (clean.local_item_id, fixture.folder(), fixture.store.clone());

    let seen = fixture.run_on_write_operation_thread(move || {
        let freed = free_up.write_not_on_disk(id).expect("the row is written");
        let presence = store.call_blocking(move |store| store.item_store().reader().find_item(id)).expect("the rows").expect("the row").content_presence;
        let seen = (presence, folder.join("report.txt").is_file());
        assert!(matches!(free_up.remove_freed_copy(freed), FreedCopy::Freed { .. }));
        seen
    });

    assert_eq!(seen, (ContentPresence::NotOnDisk, true), "the row says not on disk while the copy is still there");
    assert_eq!(fixture.read_disk_file("report.txt"), None);
}

#[test]
fn a_death_between_the_row_and_the_copys_going_leaves_a_copy_the_check_at_start_reads_as_landed() {
    let fixture = ItemFixture::new();
    let clean = make_clean_copy(&fixture, "report.txt", b"the report");
    let (free_up, id) = (make_free_up(&fixture), clean.local_item_id);
    fixture.run_on_write_operation_thread(move || drop(free_up.write_not_on_disk(id).expect("the row is written")));
    assert_eq!(presence_of(&fixture, &clean), ContentPresence::NotOnDisk);

    fixture.run_start_check();

    assert_eq!(presence_of(&fixture, &clean), ContentPresence::OnDisk, "the copy, landed before its row");
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
    fixture.assert_rows_and_disk_agree();
    assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 10)), Vec::new(), "nothing is to send that nobody did");
}

#[test]
fn a_copy_changed_between_the_decision_and_the_commit_stays() {
    // What the commit asks for itself, whatever was decided: the content it is given is
    // the row's and the synced one, and no writer began to change it.
    let fixture = ItemFixture::new();
    let clean = make_clean_copy(&fixture, "report.txt", b"the report");
    let (id, decided) = (clean.local_item_id, clean.local.clone().unwrap().content.unwrap());
    let changed = fixture.change_file_here(id, b"changed here");
    assert_ne!(changed.local.unwrap().content.unwrap(), decided);

    let committed = fixture.write_rows(move |store| store.item_store().commit_freed_content(id, &decided));

    assert_eq!(committed, StepCommitOutcome::RowsChanged);
    assert_eq!(presence_of(&fixture, &clean), ContentPresence::OnDisk);
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"changed here"[..]));
}

#[test]
fn a_copy_that_cannot_be_removed_is_on_disk_in_its_row_again() {
    let fixture = ItemFixture::new();
    let clean = make_clean_copy(&fixture, "report.txt", b"the report");
    let free_up = make_free_up(&fixture);
    // Nothing can be moved out of, or into, the root.
    fs::set_permissions(fixture.folder(), fs::Permissions::from_mode(0o555)).unwrap();

    let freed = free(&fixture, &free_up, &clean);

    fs::set_permissions(fixture.folder(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(freed, FreedCopy::RowsChanged);
    assert_eq!(presence_of(&fixture, &clean), ContentPresence::OnDisk);
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_clean_row_that_says_on_disk_over_no_file_is_mended_to_not_on_disk() {
    let fixture = ItemFixture::new();
    let (missing, there) = (make_clean_copy(&fixture, "missing.txt", b"missing"), make_clean_copy(&fixture, "there.txt", b"there"));
    fs::remove_file(fixture.disk_path("missing.txt")).unwrap();
    let free_up = make_free_up(&fixture);

    assert!(mend(&fixture, &free_up, &missing));
    assert_eq!(presence_of(&fixture, &missing), ContentPresence::NotOnDisk);
    fixture.assert_rows_and_disk_agree();

    assert!(!mend(&fixture, &free_up, &there), "a copy that is on disk is no missing copy");
    assert_eq!(presence_of(&fixture, &there), ContentPresence::OnDisk);
    assert!(!mend(&fixture, &free_up, &missing), "nothing is to mend twice");
}

#[test]
fn a_changed_row_that_says_on_disk_over_no_file_is_left_for_the_check_at_start() {
    let fixture = ItemFixture::new();
    let changed = fixture.change_file_here(make_clean_copy(&fixture, "report.txt", b"the report").local_item_id, b"changed here");
    let own = fixture.make_own_file("own.txt", b"own");
    fs::remove_file(fixture.disk_path("report.txt")).unwrap();
    fs::remove_file(fixture.disk_path("own.txt")).unwrap();
    let free_up = make_free_up(&fixture);

    assert!(!mend(&fixture, &free_up, &changed));
    assert!(!mend(&fixture, &free_up, &own));
    assert_eq!(presence_of(&fixture, &changed), ContentPresence::OnDisk, "left as it is: the check at start reads a removal by hand");
    assert_eq!(fixture.find_item(changed.local_item_id), Some(changed));
}
