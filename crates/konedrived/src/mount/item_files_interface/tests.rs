use std::fs;

use konedrive_tree::Item;

use super::*;
use crate::mount::item_download_landing::ItemDownloadLanding;
use crate::mount::item_fixture::{ContentRefusal, ItemFixture};
use crate::status::report::Report;

fn make_files_interface(fixture: &ItemFixture) -> ItemFilesInterface {
    let landing = Arc::new(ItemDownloadLanding::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.running_steps), Arc::clone(&fixture.mount_take_down)));
    let downloader = Arc::new(ItemDownloader::new(
        Arc::clone(&fixture.backing_directory),
        Arc::clone(&fixture.download_states),
        landing,
        Arc::clone(&fixture.inotify_bridge),
        Arc::clone(&fixture.write_operation_thread),
        fixture.content_source(),
        Arc::clone(&fixture.pool),
        Arc::clone(&fixture.split_download_share),
        Report::nowhere(),
        fixture.folder(),
        fixture.runtime(),
    ));
    let free_up = Arc::new(ItemFreeUp::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.item_handles), Arc::clone(&fixture.download_states), Arc::clone(&fixture.running_steps), Arc::clone(&fixture.inotify_bridge)));
    ItemFilesInterface::new(fixture.folder(), fixture.store.clone(), Arc::clone(&fixture.download_states), downloader, free_up, Arc::clone(&fixture.item_handles), Arc::clone(&fixture.write_operation_thread), Arc::clone(&fixture.sync_plan_wake), Arc::clone(&fixture.inotify_bridge), Arc::clone(&fixture.running_steps))
}

fn make_clean_copy(fixture: &ItemFixture, path: &str, content: &[u8]) -> Item {
    let shown = fixture.show_onedrive_file(path, content);
    fixture.land_copy(shown.local_item_id, content);
    fixture.find_item(shown.local_item_id).expect("the row")
}

fn presence_at(fixture: &ItemFixture, path: &str) -> ContentPresence {
    fixture.find_item_at(path).expect("the row").content_presence
}

#[test]
fn a_path_outside_the_folder_and_a_path_the_folder_does_not_show_are_refused() {
    let fixture = ItemFixture::new();
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();

    let outside = fixture.directory.path().join("elsewhere.txt");
    assert!(matches!(run.block_on(files.find_path_status(&outside)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(run.block_on(files.download(&outside)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(run.block_on(files.free_up(&outside)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(run.block_on(files.set_pin(&outside, true)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(run.block_on(files.count_downloaded_below(&outside)), Err(FilesRefusal::OutsideFolder)));

    let not_shown = fixture.disk_path("no such name.txt");
    assert!(matches!(run.block_on(files.find_path_status(&not_shown)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.download(&not_shown)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.free_up(&not_shown)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.set_pin(&not_shown, true)), Err(FilesRefusal::NotInOneDrive)));
}

#[test]
fn an_item_with_no_binding_is_not_in_onedrive() {
    let fixture = ItemFixture::new();
    fixture.make_own_file("own.txt", b"own");
    let files = make_files_interface(&fixture);
    let (run, own) = (fixture.runtime(), fixture.disk_path("own.txt"));

    assert!(matches!(run.block_on(files.find_path_status(&own)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.download(&own)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.free_up(&own)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.set_pin(&own, true)), Err(FilesRefusal::NotInOneDrive)));
    assert_eq!(fixture.read_disk_file("own.txt").as_deref(), Some(&b"own"[..]));
}

#[test]
fn a_download_asked_by_path_is_the_items_download_and_is_awaited() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_file("docs/report.txt", b"the report");
    let files = make_files_interface(&fixture);

    fixture.runtime().block_on(files.download(&fixture.disk_path("docs/report.txt"))).expect("the file is downloaded");

    assert_eq!(fixture.read_disk_file("docs/report.txt").as_deref(), Some(&b"the report"[..]), "landed when the call returns");
    assert_eq!(presence_at(&fixture, "docs/report.txt"), ContentPresence::OnDisk);
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 1);

    // On disk already: nothing is asked of OneDrive.
    fixture.runtime().block_on(files.download(&fixture.disk_path("docs/report.txt"))).expect("nothing to download is no failure");
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 1);
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_is_downloaded_file_by_file_and_a_failure_is_said_after_the_rest() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_directory("docs/deep");
    fixture.show_onedrive_file("docs/a.txt", b"a");
    let gone = fixture.show_onedrive_file("docs/gone.txt", b"gone");
    fixture.show_onedrive_file("docs/deep/b.txt", b"bb");
    make_clean_copy(&fixture, "docs/there.txt", b"there");
    fixture.show_onedrive_file("outside.txt", b"outside");
    fixture.refuse_content(gone.local_item_id, Some(ContentRefusal::NotFound));
    let files = make_files_interface(&fixture);

    let downloaded = fixture.runtime().block_on(files.download(&fixture.disk_path("docs")));

    assert!(matches!(downloaded, Err(FilesRefusal::Io(_))), "the file OneDrive does not have: {downloaded:?}");
    assert_eq!(fixture.read_disk_file("docs/a.txt").as_deref(), Some(&b"a"[..]));
    assert_eq!(fixture.read_disk_file("docs/deep/b.txt").as_deref(), Some(&b"bb"[..]), "every file below it, at any depth");
    assert_eq!(fixture.read_disk_file("docs/gone.txt"), None);
    assert_eq!(fixture.read_disk_file("outside.txt"), None, "nothing that is not below it");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_is_freed_file_by_file_each_by_its_own_decision() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_directory("docs/deep");
    make_clean_copy(&fixture, "docs/clean.txt", &[1; 5000]);
    make_clean_copy(&fixture, "docs/deep/clean.txt", &[2; 5000]);
    let changed = make_clean_copy(&fixture, "docs/changed.txt", b"changed");
    fixture.change_file_here(changed.local_item_id, b"changed here");
    let pinned = make_clean_copy(&fixture, "docs/pinned.txt", b"pinned");
    let pinned_id = pinned.local_item_id;
    fixture.write_rows(move |store| store.item_store().set_item_pin(pinned_id, true));
    fixture.show_onedrive_file("docs/remote.txt", b"remote");
    fixture.make_own_file("docs/own.txt", b"own");
    make_clean_copy(&fixture, "outside.txt", b"outside");
    let files = make_files_interface(&fixture);

    let freed = fixture.runtime().block_on(files.free_up(&fixture.disk_path("docs"))).expect("the directory is freed");

    assert_eq!((freed.files, freed.kept), (2, 3), "two clean copies freed; the changed, the pinned and the user's own file kept");
    assert!(freed.bytes >= 8192, "the bytes the two copies took: {}", freed.bytes);
    assert_eq!(fixture.read_disk_file("docs/clean.txt"), None);
    assert_eq!(fixture.read_disk_file("docs/deep/clean.txt"), None);
    assert_eq!(fixture.read_disk_file("docs/changed.txt").as_deref(), Some(&b"changed here"[..]));
    assert_eq!(fixture.read_disk_file("docs/pinned.txt").as_deref(), Some(&b"pinned"[..]));
    assert_eq!(fixture.read_disk_file("outside.txt").as_deref(), Some(&b"outside"[..]));
    fixture.assert_rows_and_disk_agree();

    // The whole folder: what is outside the directory too.
    let freed = fixture.runtime().block_on(files.free_up_all()).expect("the folder is freed");
    assert_eq!((freed.files, freed.kept), (1, 3));
    assert_eq!(fixture.read_disk_file("outside.txt"), None);

    // One file, by its own path.
    make_clean_copy(&fixture, "single.txt", b"single");
    let freed = fixture.runtime().block_on(files.free_up(&fixture.disk_path("single.txt"))).expect("the file is freed");
    assert_eq!((freed.files, freed.kept), (1, 0));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_free_up_after_the_thread_stopped_is_refused() {
    let fixture = ItemFixture::new();
    make_clean_copy(&fixture, "report.txt", b"the report");
    let files = make_files_interface(&fixture);
    fixture.write_operation_thread.stop();

    let freed = fixture.runtime().block_on(files.free_up_all());

    assert!(matches!(freed, Err(FilesRefusal::Io(_))), "the folder is no longer served: {freed:?}");
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]));
}

#[test]
fn a_pin_is_set_by_the_stores_job_and_the_plan_woken() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_file("docs/report.txt", b"the report");
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    assert_eq!(run.block_on(files.count_pins()), 0);

    run.block_on(files.set_pin(&fixture.disk_path("docs"), true)).expect("the pin is set");

    assert_eq!(run.block_on(fixture.sync_plan_wake.wait()), [WakeCause::UserAnswered].into(), "the plan is woken: it downloads what the pin holds");
    assert_eq!(run.block_on(files.count_pins()), 1);
    let below = run.block_on(files.find_path_status(&fixture.disk_path("docs/report.txt"))).expect("the status");
    assert!(below.is_pinned, "the pin of the directory holds for what is in it");

    run.block_on(files.set_pin(&fixture.disk_path("docs"), false)).expect("the pin is taken off");
    assert_eq!(run.block_on(files.count_pins()), 0);
    assert!(!run.block_on(files.find_path_status(&fixture.disk_path("docs/report.txt"))).expect("the status").is_pinned);
}

#[test]
fn the_status_of_a_path_is_the_rows_sync_state_its_pin_and_what_runs_for_it() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let remote = fixture.show_onedrive_file("docs/remote.txt", b"remote");
    let clean = make_clean_copy(&fixture, "docs/clean.txt", b"clean");
    let changed = fixture.change_file_here(make_clean_copy(&fixture, "docs/changed.txt", b"changed").local_item_id, b"changed here");
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    let status = |path: &str| run.block_on(files.find_path_status(&fixture.disk_path(path))).expect("the status");

    assert_eq!(status("docs"), ItemPathStatus { item: directory.local_item_id, kind: ItemKind::Directory, onedrive_item: ItemFixture::onedrive_reference_of("docs"), sync_state: None, is_pinned: false, pin_holder: super::PinHolder::Nobody, is_downloading: false, is_open_for_writing: false });
    assert_eq!(status("docs/remote.txt").sync_state, Some(FileSyncState::RemoteOnly));
    assert_eq!(status("docs/clean.txt").sync_state, Some(FileSyncState::Synced));
    assert_eq!(status("docs/changed.txt").sync_state, Some(FileSyncState::ModifiedHere));
    assert_eq!(status("docs/changed.txt").item, changed.local_item_id);
    assert_eq!(status("").kind, ItemKind::Directory, "the folder itself");

    // What runs for it: a download, a writer.
    let key = crate::mount::item_download_states::DownloadKey { item: remote.local_item_id, content: remote.local.clone().unwrap().content.unwrap() };
    let _started = fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None);
    let (writer, _) = fixture.item_handles.add_writer(clean.local_item_id, Arc::new(fs::File::open(fixture.disk_path("docs/clean.txt")).unwrap()));
    assert!(status("docs/remote.txt").is_downloading && !status("docs/remote.txt").is_open_for_writing);
    assert!(status("docs/clean.txt").is_open_for_writing && !status("docs/clean.txt").is_downloading);
    fixture.item_handles.remove_writer(writer);
}

#[test]
fn what_is_downloaded_below_a_path_is_counted_from_the_rows() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    make_clean_copy(&fixture, "docs/a.txt", &[1; 100]);
    make_clean_copy(&fixture, "docs/b.txt", &[2; 23]);
    fixture.show_onedrive_file("docs/remote.txt", &[3; 1000]);
    make_clean_copy(&fixture, "outside.txt", &[4; 7]);
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    // The backing directory is not walked: what the rows say counts.
    fs::remove_file(fixture.disk_path("docs/b.txt")).unwrap();

    assert_eq!(run.block_on(files.count_downloaded_below(&fixture.disk_path("docs"))).unwrap(), DownloadedSpace { files: 2, bytes: 123 });
    assert_eq!(run.block_on(files.count_downloaded_below(&fixture.folder())).unwrap(), DownloadedSpace { files: 3, bytes: 130 });
    assert_eq!(run.block_on(files.count_downloaded_below(&fixture.disk_path("docs/a.txt"))).unwrap(), DownloadedSpace { files: 1, bytes: 100 });
    assert_eq!(run.block_on(files.count_downloaded_below(&fixture.disk_path("docs/remote.txt"))).unwrap(), DownloadedSpace::default());
}

/// The status of a file with `sync_state`, as far as the state for the menu reads it.
fn status_with(sync_state: Option<FileSyncState>, is_downloading: bool) -> ItemPathStatus {
    ItemPathStatus { item: LocalItemId::new(5), kind: if sync_state.is_some() { ItemKind::File } else { ItemKind::Directory }, onedrive_item: ItemFixture::onedrive_reference_of("x"), sync_state, is_pinned: false, pin_holder: PinHolder::Nobody, is_downloading, is_open_for_writing: false }
}

#[test]
fn the_state_a_path_is_shown_as_is_what_the_state_attribute_says() {
    use konedrive_fs::placeholder::State;
    let cases = [
        (None, false, None),
        (None, true, None),
        (Some(FileSyncState::LocalOnly), false, None),
        (Some(FileSyncState::LocalOnly), true, None),
        (Some(FileSyncState::RemoteOnly), false, Some(State::OnlineOnly)),
        (Some(FileSyncState::Synced), false, Some(State::Hydrated)),
        (Some(FileSyncState::ModifiedHere), false, Some(State::Hydrated)),
        (Some(FileSyncState::RemoteOnly), true, Some(State::Hydrating)),
        (Some(FileSyncState::Synced), true, Some(State::Hydrating)),
        (Some(FileSyncState::ModifiedHere), true, Some(State::Hydrating)),
    ];
    for (sync_state, is_downloading, state) in cases {
        assert_eq!(status_with(sync_state, is_downloading).find_state_attribute(), state, "{sync_state:?}, downloading: {is_downloading}");
    }

    // And of the rows: a file not on disk, the same while it is being downloaded, a copy, a directory.
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let remote = fixture.show_onedrive_file("docs/remote.txt", b"remote");
    make_clean_copy(&fixture, "docs/clean.txt", b"clean");
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    let state = |path: &str| run.block_on(files.find_path_status(&fixture.disk_path(path))).expect("the status").find_state_attribute();
    assert_eq!([state("docs"), state("docs/remote.txt"), state("docs/clean.txt")], [None, Some(State::OnlineOnly), Some(State::Hydrated)]);
    let key = crate::mount::item_download_states::DownloadKey { item: remote.local_item_id, content: remote.local.clone().unwrap().content.unwrap() };
    let _started = fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None);
    assert_eq!(state("docs/remote.txt"), Some(State::Hydrating));
}

fn running_step(item: LocalItemId, class: crate::sync_parts::running_steps::StepClass) -> crate::sync_parts::running_steps::RunningStep {
    crate::sync_parts::running_steps::RunningStep { class, item, other_items: Vec::new(), destination: None, sent: 0, total: 0 }
}

#[test]
fn whose_pin_holds_for_a_path_is_the_nearest_directorys_above_before_its_own() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    fixture.show_onedrive_directory("docs/deep");
    let file = fixture.show_onedrive_file("docs/deep/report.txt", b"the report");
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    let holder = |path: &str| {
        let status = run.block_on(files.find_path_status(&fixture.disk_path(path))).expect("the status");
        assert_eq!(status.is_pinned, status.pin_holder != PinHolder::Nobody, "{path}");
        status.pin_holder
    };
    let above = |path: &str| PinHolder::DirectoryAbove { path: fixture.disk_path(path) };
    assert_eq!(holder("docs/deep/report.txt"), PinHolder::Nobody);

    // The file's own pin; set again, nothing changed. Its change is announced: the emblem is read from the pin.
    assert!(run.block_on(files.set_pin(&fixture.disk_path("docs/deep/report.txt"), true)).unwrap());
    assert!(!run.block_on(files.set_pin(&fixture.disk_path("docs/deep/report.txt"), true)).unwrap(), "the pin was there already");
    let place = file.local.clone().unwrap().place.unwrap();
    assert_eq!(fixture.inotify_bridge.list_queued(), [vec![ItemAnnouncement::MarksChanged { item: file.local_item_id, place }]], "once: what did not change is not announced");
    assert_eq!([holder("docs/deep/report.txt"), holder("docs/deep"), holder("docs")], [PinHolder::Itself, PinHolder::Nobody, PinHolder::Nobody]);

    // A directory above comes before the item's own pin: the nearest one, by its full path.
    assert!(run.block_on(files.set_pin(&fixture.disk_path("docs"), true)).unwrap());
    assert_eq!([holder("docs/deep/report.txt"), holder("docs/deep"), holder("docs")], [above("docs"), above("docs"), PinHolder::Itself]);
    assert!(run.block_on(files.set_pin(&fixture.disk_path("docs/deep"), true)).unwrap());
    assert_eq!([holder("docs/deep/report.txt"), holder("docs/deep")], [above("docs/deep"), above("docs")]);

    // Taken off above: the file's own pin holds again; taken off twice, nothing changed.
    assert!(run.block_on(files.set_pin(&fixture.disk_path("docs"), false)).unwrap());
    assert!(!run.block_on(files.set_pin(&fixture.disk_path("docs"), false)).unwrap());
    assert!(run.block_on(files.set_pin(&fixture.disk_path("docs/deep"), false)).unwrap());
    assert_eq!([holder("docs/deep/report.txt"), holder("docs/deep"), holder("docs")], [PinHolder::Itself, PinHolder::Nobody, PinHolder::Nobody]);
}

/// What a free-up would decide is asked for every file, then the free-up runs for each:
/// the two agree, a case per decision.
#[test]
fn what_a_free_up_would_decide_is_what_the_free_up_then_does() {
    use crate::sync_parts::running_steps::StepClass;
    let fixture = ItemFixture::new();
    for directory in ["docs", "kept", "renamed"] {
        fixture.show_onedrive_directory(directory);
    }
    make_clean_copy(&fixture, "docs/clean.txt", b"clean");
    let changed = make_clean_copy(&fixture, "docs/changed.txt", b"changed");
    fixture.change_file_here(changed.local_item_id, b"changed here");
    let own_pin = make_clean_copy(&fixture, "docs/own-pin.txt", b"pinned").local_item_id;
    make_clean_copy(&fixture, "kept/below-a-pin.txt", b"below");
    let both_pins = make_clean_copy(&fixture, "kept/both-pins.txt", b"both").local_item_id;
    let kept = fixture.find_item_at("kept").unwrap().local_item_id;
    fixture.show_onedrive_file("docs/remote.txt", b"remote");
    fixture.show_onedrive_file("kept/remote.txt", b"remote below a pin");
    let downloading = fixture.show_onedrive_file("docs/downloading.txt", b"being downloaded");
    let sent = make_clean_copy(&fixture, "docs/sent.txt", b"sent").local_item_id;
    let written = make_clean_copy(&fixture, "docs/written.txt", b"written").local_item_id;
    let renamed = fixture.find_item_at("renamed").unwrap().local_item_id;
    fixture.write_rows(move |store| {
        for item in [own_pin, both_pins, kept] {
            store.item_store().set_item_pin(item, true)?;
        }
        Ok(())
    });
    // What runs: a download of a file not on disk, a step towards OneDrive for a copy and for a directory, a writer.
    let key = crate::mount::item_download_states::DownloadKey { item: downloading.local_item_id, content: downloading.local.clone().unwrap().content.unwrap() };
    let _started = fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None);
    assert!(fixture.running_steps.begin(running_step(sent, StepClass::Metadata)) && fixture.running_steps.begin(running_step(renamed, StepClass::Metadata)));
    let (writer, _) = fixture.item_handles.add_writer(written, Arc::new(fs::File::open(fixture.disk_path("docs/written.txt")).unwrap()));
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    let decide = |path: &str, is_own_pin_taken_off| run.block_on(files.decide_free_up(&fixture.disk_path(path), is_own_pin_taken_off)).unwrap_or_else(|refusal| panic!("{path}: {refusal}"));

    // A directory: by the pin that holds for its own row, and nothing else — not a step that runs for it.
    assert_eq!([decide("docs", false), decide("renamed", false), decide("kept", false), decide("kept", true)], [FreeUpDecision::NothingToFree, FreeUpDecision::NothingToFree, FreeUpDecision::LeavePinned, FreeUpDecision::NothingToFree]);
    // With the item's own pin taken off first, as `Files.FreeUp` takes it off: a pin above still holds.
    assert_eq!([decide("docs/own-pin.txt", true), decide("kept/both-pins.txt", true), decide("kept/below-a-pin.txt", true)], [FreeUpDecision::Free, FreeUpDecision::LeavePinned, FreeUpDecision::LeavePinned]);

    let cases = [
        ("docs/clean.txt", FreeUpDecision::Free),
        ("docs/changed.txt", FreeUpDecision::LeaveModifiedHere),
        ("docs/own-pin.txt", FreeUpDecision::LeavePinned),
        ("kept/below-a-pin.txt", FreeUpDecision::LeavePinned),
        ("kept/both-pins.txt", FreeUpDecision::LeavePinned),
        ("docs/sent.txt", FreeUpDecision::LeaveDownloading),
        ("docs/written.txt", FreeUpDecision::LeaveOpenForWriting),
        // What is not on disk takes no space, whatever keeps it: a pin above, a download.
        ("docs/remote.txt", FreeUpDecision::NothingToFree),
        ("kept/remote.txt", FreeUpDecision::NothingToFree),
        ("docs/downloading.txt", FreeUpDecision::NothingToFree),
    ];
    for (path, decision) in cases {
        assert_eq!(decide(path, false), decision, "{path}");
        let freed = run.block_on(files.free_up(&fixture.disk_path(path))).expect("the free-up");
        let done = match decision {
            FreeUpDecision::Free => (1, 0),
            FreeUpDecision::NothingToFree => (0, 0),
            FreeUpDecision::LeavePinned | FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting | FreeUpDecision::LeaveDownloading => (0, 1),
        };
        assert_eq!((freed.files, freed.kept), done, "the free-up of {path} does what was decided: {decision:?}");
    }
    assert_eq!(decide("docs/clean.txt", false), FreeUpDecision::NothingToFree, "freed: nothing more to free");
    fixture.item_handles.remove_writer(writer);

    // What the folder does not show, and what OneDrive does not have, is refused as every asking by path.
    fixture.make_own_file("own.txt", b"own");
    assert!(matches!(run.block_on(files.decide_free_up(&fixture.disk_path("own.txt"), false)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.decide_free_up(&fixture.disk_path("no such name"), false)), Err(FilesRefusal::NotInOneDrive)));
    assert!(matches!(run.block_on(files.decide_free_up(&fixture.directory.path().join("elsewhere"), false)), Err(FilesRefusal::OutsideFolder)));
}

#[test]
fn the_unfinished_downloads_are_listed_by_path_with_their_size_and_checkpoint() {
    use std::io::Write as _;
    use konedrive_fs::placeholder::{write_progress, Progress};
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let files = make_files_interface(&fixture);
    let run = fixture.runtime();
    assert_eq!(run.block_on(files.list_unfinished_downloads()).unwrap(), []);

    // A file a download wrote `written` into, beside the folder: no file of the downloads has a name in it.
    let unfinished_file = |name: &str, written: &[u8]| {
        let mut file = fs::OpenOptions::new().create_new(true).read(true).write(true).open(fixture.directory.path().join(name)).unwrap();
        file.write_all(written).unwrap();
        Arc::new(file)
    };
    let start = |path: &str, content: &[u8]| {
        let shown = fixture.show_onedrive_file(path, content);
        let key = crate::mount::item_download_states::DownloadKey { item: shown.local_item_id, content: shown.local.clone().unwrap().content.unwrap() };
        match fixture.download_states.start_or_join(&key, &DownloadAsker::Read, None) {
            crate::mount::item_download_states::ItemDownloadStart::Started { run, .. } => (shown.local_item_id, run),
            _ => panic!("no download of {path} runs"),
        }
    };
    // One that runs, with a checkpoint; one that failed, with none; one that has no file yet; one whose file is whole.
    let (_, running) = start("docs/running.bin", &[1; 100]);
    let running_file = unfinished_file("running", &[1; 40]);
    write_progress(&running_file, &Progress { ctag: "c-running".into(), bytes: 32 }).unwrap();
    fixture.download_states.begin_writing(&running, running_file);
    let (_, failed) = start("docs/failed.bin", &[2; 100]);
    fixture.download_states.begin_writing(&failed, unfinished_file("failed", &[2; 7]));
    fixture.download_states.finish(&failed, Err(libc::EIO));
    let _no_file_yet = start("docs/no-file-yet.bin", &[3; 100]);
    let (_, whole) = start("docs/whole.bin", &[4; 100]);
    fixture.download_states.begin_writing(&whole, unfinished_file("whole", &[4; 100]));
    fixture.download_states.end_writing(&whole);
    // And one whose file the folder shows no more: it has no path to be listed under.
    let (removed, gone) = start("docs/removed.bin", &[5; 100]);
    fixture.download_states.begin_writing(&gone, unfinished_file("removed", &[5; 9]));
    fixture.write_rows(move |store| store.item_store().apply(&konedrive_tree::LocalVersionChange::Removed { item: removed }).map(drop));

    let mut unfinished = run.block_on(files.list_unfinished_downloads()).unwrap();
    unfinished.sort();
    assert_eq!(unfinished, [(fixture.disk_path("docs/failed.bin"), 7, None), (fixture.disk_path("docs/running.bin"), 40, Some(32))]);
}
