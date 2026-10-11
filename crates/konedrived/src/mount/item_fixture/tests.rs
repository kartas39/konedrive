//! The fixture itself: what each builder leaves in the rows and on disk, the threads, the
//! parts it hands out, and the stand-in's commits.

use std::sync::atomic::Ordering;

use konedrive_tree::{ContentPresence, ContentVersion, FileSyncState, FolderStep, ItemKind, LocalItemId, OneDriveStep, SyncStep};

use super::{make_stand_in_hash, onedrive_id_of, ContentFetch, ContentRefusal, DiskMarksRead, ItemFixture, DRIVE_ID, ONEDRIVE_MTIME, ROOT_ONEDRIVE_ID, START_TIME};
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepOutcome, StepRefusal};
use crate::mount::mount_take_down::TakeDownCause;
use crate::mount::place_changes::OpenMode;
use crate::content_source::source::SourceError;

/// One of each thing a builder makes: an agreed directory and an agreed file with their
/// copies on disk, a file and a directory that are shown and not on disk, a new local
/// file and directory, and an item new in OneDrive.
fn make_one_of_each(fixture: &ItemFixture) {
    fixture.show_onedrive_directory("docs");
    let copy = fixture.show_onedrive_file("docs/copy.txt", b"the copy");
    fixture.land_copy(copy.local_item_id, b"the copy");
    fixture.show_onedrive_directory("empty");
    fixture.show_onedrive_file("empty/elsewhere.txt", b"not here");
    fixture.make_own_directory("mine");
    fixture.make_own_file("mine/new.txt", b"new here");
    fixture.make_own_file("docs/also-new.txt", b"new in a directory of OneDrive");
    fixture.list_onedrive_file("docs/new-in-onedrive.txt", b"new there");
    fixture.list_onedrive_directory("new-directory-in-onedrive");
    // What a program did since, and what OneDrive did.
    let changed = fixture.show_onedrive_file("changed-here.txt", b"as it was");
    fixture.land_copy(changed.local_item_id, b"as it was");
    fixture.change_file_here(changed.local_item_id, b"as a program left it");
    let moved = fixture.show_onedrive_file("moved-here.txt", b"moved");
    fixture.land_copy(moved.local_item_id, b"moved");
    fixture.move_here(moved.local_item_id, "mine/moved-here.txt");
    let removed = fixture.show_onedrive_file("removed-here.txt", b"removed");
    fixture.land_copy(removed.local_item_id, b"removed");
    fixture.remove_here(removed.local_item_id);
    fixture.show_onedrive_file("moved-in-onedrive.txt", b"moved there");
    fixture.list_onedrive_move("moved-in-onedrive.txt", "docs/moved-in-onedrive.txt");
}

#[test]
fn the_fixture_is_built_an_item_made_read_through_a_read_connection_and_the_check_is_clean() {
    let fixture = ItemFixture::new();
    let root = fixture.find_item(LocalItemId::ROOT).expect("the root has a row");
    assert_eq!(root.onedrive_item, Some(ItemFixture::onedrive_reference_of("")));
    assert_eq!((DRIVE_ID, ROOT_ONEDRIVE_ID), ("d1", "R"));

    let made = fixture.show_onedrive_file("a.txt", b"a");

    let id = made.local_item_id;
    let read = fixture.read_through_read_connection(move |reader| Ok((reader.find_item(id)?, reader.find_path(id)?))).unwrap();
    assert_eq!(read, (Some(made), Some("a.txt".into())));
    let findings = fixture.run_start_check();
    assert!(findings.held_items.is_empty() && !findings.has_unread_part, "{findings:?}");
    assert_eq!((findings.unresolved_path_count, findings.bound_by_mark_count), (0, 0));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn each_builder_leaves_the_row_and_the_disk_its_doc_says() {
    let fixture = ItemFixture::new();
    make_one_of_each(&fixture);

    let directory = fixture.find_item_at("docs").unwrap();
    assert_eq!((directory.kind, directory.onedrive_item.clone()), (ItemKind::Directory, Some(ItemFixture::onedrive_reference_of("docs"))));
    assert_eq!(fixture.read_disk_marks("docs").onedrive_item_id.as_deref(), Some("docs"), "made on disk by the copy that landed in it, and marked");

    let copy = fixture.find_item_at("docs/copy.txt").unwrap();
    let content = ItemFixture::onedrive_content_of(b"the copy");
    assert_eq!((copy.file_sync_state(), copy.content_presence), (Some(FileSyncState::Synced), ContentPresence::OnDisk));
    assert_eq!((copy.local.as_ref().unwrap().content.as_ref(), copy.synced_content.as_ref(), copy.onedrive.as_ref().unwrap().content.as_ref()), (Some(&content), Some(&content), Some(&content)));
    assert_eq!(fixture.read_disk_file("docs/copy.txt").as_deref(), Some(&b"the copy"[..]));
    let ContentVersion::Hashed { ctag, .. } = &content else { unreachable!() };
    assert_eq!(fixture.read_disk_marks("docs/copy.txt"), DiskMarksRead { onedrive_item_id: Some(onedrive_id_of("docs/copy.txt")), is_whole: true, ctag: ctag.clone() });
    assert_eq!(std::fs::metadata(fixture.disk_path("docs/copy.txt")).unwrap().modified().unwrap(), std::time::UNIX_EPOCH + std::time::Duration::from_secs(ONEDRIVE_MTIME as u64));

    let elsewhere = fixture.find_item_at("empty/elsewhere.txt").unwrap();
    assert_eq!((elsewhere.file_sync_state(), elsewhere.content_presence), (Some(FileSyncState::RemoteOnly), ContentPresence::NotOnDisk));
    assert!(!fixture.disk_path("empty").exists(), "a shown directory is not on disk until something is put into it");

    let new = fixture.find_item_at("mine/new.txt").unwrap();
    assert_eq!((new.file_sync_state(), new.onedrive_item.clone(), new.content_presence), (Some(FileSyncState::LocalOnly), None, ContentPresence::OnDisk));
    assert_eq!(new.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 8, mtime: START_TIME, mtime_nsec: 0, change_count: 1 }));
    assert_eq!(fixture.read_disk_marks("mine/new.txt"), DiskMarksRead { onedrive_item_id: None, is_whole: false, ctag: None });
    assert!(fixture.find_item_at("mine").unwrap().onedrive_item.is_none() && fixture.disk_path("mine").is_dir());

    assert_eq!(fixture.find_item_at("docs/new-in-onedrive.txt"), None, "the folder does not show what was only listed");
    let listed = fixture.read_rows(|reader| reader.find_bound_item(&ItemFixture::onedrive_reference_of("docs/new-in-onedrive.txt"))).expect("it has a row");
    assert_eq!((listed.local.clone(), listed.onedrive.as_ref().unwrap().content.clone()), (None, Some(ItemFixture::onedrive_content_of(b"new there"))));
    assert_eq!(fixture.versions_of(listed.local_item_id).onedrive, listed.onedrive);
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn what_a_program_did_and_what_onedrive_did_is_in_the_rows_as_unsent_and_as_not_yet_shown() {
    let fixture = ItemFixture::new();
    make_one_of_each(&fixture);

    let changed = fixture.find_item_at("changed-here.txt").unwrap();
    assert_eq!((changed.file_sync_state(), changed.content_presence), (Some(FileSyncState::ModifiedHere), ContentPresence::OnDisk));
    assert_eq!(changed.local.as_ref().unwrap().content.as_ref().map(|content| (content.size(), content.mtime())), Some((20, (START_TIME, 0))));
    assert_eq!(changed.synced_content, Some(ItemFixture::onedrive_content_of(b"as it was")));
    assert_eq!(fixture.read_disk_file("changed-here.txt").as_deref(), Some(&b"as a program left it"[..]));

    let moved = fixture.find_item_at("mine/moved-here.txt").unwrap();
    assert_eq!(moved.onedrive_item, Some(ItemFixture::onedrive_reference_of("moved-here.txt")));
    assert_eq!(moved.synced_place.as_ref().map(|place| place.name.clone()), Some("moved-here.txt".into()), "what was agreed on is the place before the move");
    assert!(fixture.read_disk_file("mine/moved-here.txt").is_some() && fixture.read_disk_file("moved-here.txt").is_none());

    let removed = fixture.read_rows(|reader| reader.find_bound_item(&ItemFixture::onedrive_reference_of("removed-here.txt"))).expect("the row of an item OneDrive has stays");
    assert_eq!((removed.local.clone(), removed.onedrive.is_some(), fixture.read_disk_file("removed-here.txt")), (None, true, None));
    let own = fixture.find_item_at("mine/new.txt").unwrap();
    assert_eq!(fixture.remove_here(own.local_item_id), None, "the row of an item OneDrive never had goes");

    let moved_there = fixture.find_item_at("moved-in-onedrive.txt").expect("the folder shows it where it was");
    let place_there = moved_there.onedrive.as_ref().unwrap().place.clone().unwrap();
    assert_eq!((place_there.parent_local_item_id, place_there.name), (fixture.find_item_at("docs").unwrap().local_item_id, "moved-in-onedrive.txt".into()));
    assert_eq!(moved_there.onedrive.as_ref().unwrap().content, Some(ItemFixture::onedrive_content_of(b"moved there")), "with the content it had");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn what_each_builder_makes_the_check_at_start_and_a_mounts_build_leave_as_it_is() {
    let fixture = ItemFixture::new();
    make_one_of_each(&fixture);
    let rows = fixture.list_items();

    let findings = fixture.run_start_check();
    assert!(findings.held_items.is_empty() && !findings.has_unread_part && findings.unresolved_path_count == 0 && findings.bound_by_mark_count == 0, "{findings:?}");
    assert_eq!(fixture.list_items(), rows, "the check changed no row a builder made");

    let mount = fixture.build_mount();
    assert_eq!(mount.start_check_findings(), &findings);
    assert_eq!(fixture.list_items(), rows, "nor did a mount's build");
    // Rows made after the build are the mount's parts' to read all the same.
    let later = fixture.show_onedrive_file("later.txt", b"later");
    fixture.land_copy(later.local_item_id, b"later");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_listed_again_has_another_content_in_onedrive_and_the_folder_shows_the_first() {
    let fixture = ItemFixture::new();
    let first = fixture.show_onedrive_file("a.txt", b"first");

    let listed_again = fixture.list_onedrive_file("a.txt", b"the second");

    assert_eq!(listed_again.local_item_id, first.local_item_id, "one path, one item");
    assert_eq!(listed_again.local, first.local);
    assert_eq!(listed_again.onedrive.as_ref().unwrap().content, Some(ItemFixture::onedrive_content_of(b"the second")));
    assert_ne!(listed_again.onedrive_etag, first.onedrive_etag);
    fixture.list_onedrive_removal("a.txt");
    assert_eq!(fixture.find_item(first.local_item_id).unwrap().onedrive, None, "OneDrive ceased to have it");
}

#[test]
fn a_job_runs_on_the_write_operation_thread_and_on_a_mount_like_thread_where_the_place_changes_allow_each() {
    let fixture = ItemFixture::new();
    let own = fixture.make_own_file("own.txt", b"own");
    // A place change begins on the write-operation thread alone (asserted there).
    let place_changes = std::sync::Arc::clone(&fixture.place_changes);
    let thread = fixture.run_on_write_operation_thread(move || {
        place_changes.begin().end();
        std::thread::current().id()
    });
    assert_eq!(thread, fixture.write_operation_thread.thread_id());
    // A backing file is opened by a row's path off it (asserted there), through a
    // reading connection: on the mount-like thread, the same at every call.
    let (place_changes, read_connections, id) = (std::sync::Arc::clone(&fixture.place_changes), std::sync::Arc::clone(&fixture.read_connections), own.local_item_id);
    let open = move || {
        let opened = place_changes.open_at_row_path(OpenMode::Read, || read_connections.read(|reader| Ok(reader.find_path(id)?.map(|path| (path, ())))));
        (opened.unwrap().is_some(), std::thread::current().id())
    };
    let (first, second) = (fixture.run_on_mount_like_thread(open.clone()), fixture.run_on_mount_like_thread(open));
    assert_eq!((first.0, second.0, first.1 == second.1), (true, true, true));
    assert!(first.1 != thread && first.1 != std::thread::current().id());
    assert_eq!(fixture.read_connections.count_connections(), 1);
    // A job's panic is the caller's, and the thread runs the next job.
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture.run_on_write_operation_thread(|| panic!("the job's own panic"))));
    assert!(panicked.is_err());
    assert_eq!(fixture.run_on_write_operation_thread(|| 7), 7);
}

/// Everything a piece's test does with the fixture, on the thread it is called on.
async fn use_whole_fixture() {
    let fixture = ItemFixture::new();
    make_one_of_each(&fixture);
    assert_eq!(fixture.run_on_write_operation_thread(|| 1) + fixture.run_on_mount_like_thread(|| 2), 3);
    let mount = fixture.build_mount();
    assert!(mount.start_check_findings().held_items.is_empty());
    let fake_onedrive = fixture.start_fake_onedrive().await;
    let graph = fixture.graph_access(&fake_onedrive);
    assert_eq!((graph.drive_id.as_str(), (graph.clock)()), (DRIVE_ID, START_TIME));
    let runners = fixture.step_runners_stand_in();
    let listed = fixture.read_rows(|reader| reader.find_bound_item(&ItemFixture::onedrive_reference_of("docs/new-in-onedrive.txt"))).unwrap();
    assert_eq!((runners.run_show_steps)(vec![fixture.versions_of(listed.local_item_id)]).await, [StepOutcome::Written { events: Vec::new() }]);
    fixture.assert_rows_and_disk_agree();
    mount.stop();
}

#[tokio::test]
async fn the_fixture_is_made_used_and_dropped_on_the_thread_of_a_current_thread_runtime() {
    use_whole_fixture().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_is_made_used_and_dropped_on_a_worker_of_a_runtime() {
    use_whole_fixture().await;
}

#[tokio::test]
async fn the_fake_onedrive_holds_what_the_rows_say_onedrive_has() {
    let fixture = ItemFixture::new();
    make_one_of_each(&fixture);

    let fake_onedrive = fixture.start_fake_onedrive().await;

    let paths = fake_onedrive.with(|cloud| cloud.paths());
    for path in ["docs", "docs/copy.txt", "empty/elsewhere.txt", "docs/new-in-onedrive.txt", "new-directory-in-onedrive"] {
        assert!(paths.iter().any(|held| held.trim_start_matches('/') == path), "{path} is in {paths:?}");
    }
    assert!(!paths.iter().any(|held| held.contains("new.txt") || held.contains("mine")), "what OneDrive never had is not there: {paths:?}");
    let row = fixture.find_item_at("docs/copy.txt").unwrap();
    let held = fake_onedrive.with(|cloud| cloud.item(&onedrive_id_of("docs/copy.txt")).cloned()).expect("the file by its id");
    let ContentVersion::Hashed { hash, ctag, .. } = ItemFixture::onedrive_content_of(b"the copy") else { unreachable!() };
    assert_eq!((held.content, held.hash, Some(held.ctag), Some(held.etag), held.parent.as_deref(), held.mtime), (b"the copy".to_vec(), hash, ctag, row.onedrive_etag, Some("docs"), ONEDRIVE_MTIME));
    assert_eq!(fake_onedrive.with(|cloud| cloud.item("docs").map(|item| (item.folder, item.parent.clone()))), Some((true, Some(ROOT_ONEDRIVE_ID.to_owned()))));
}

#[test]
fn the_content_source_serves_what_was_listed_or_given_and_refuses_as_asked() {
    use tokio::io::AsyncReadExt;
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"0123456789");
    let (source, id) = (fixture.content_source(), onedrive_id_of("a.txt"));
    let fetch = |from: u64, end: Option<u64>| {
        let (source, id) = (std::sync::Arc::clone(&source), id.clone());
        fixture.runtime().block_on(async move {
            let mut fetched = source.fetch(&id, from, end).await?;
            let mut bytes = Vec::new();
            fetched.stream.read_to_end(&mut bytes).await.unwrap();
            Ok::<_, SourceError>((fetched.served_from, fetched.size, bytes, fetched.version.map(|version| version.ctag)))
        })
    };
    let ContentVersion::Hashed { ctag, .. } = ItemFixture::onedrive_content_of(b"0123456789") else { unreachable!() };

    assert_eq!(fetch(0, None).unwrap(), (0, Some(10), b"0123456789".to_vec(), ctag));
    assert_eq!(fetch(3, Some(6)).unwrap().2, b"345");
    assert_eq!(fetch(10, None).unwrap().2, b"", "a fetch from the end is answered, empty");
    fixture.serve_content(file.local_item_id, b"other bytes");
    assert_eq!(fetch(0, None).unwrap().2, b"other bytes");
    fixture.refuse_content(file.local_item_id, Some(ContentRefusal::Transient));
    assert!(matches!(fetch(0, None), Err(SourceError::Transient(_))));
    fixture.refuse_content(file.local_item_id, Some(ContentRefusal::NotFound));
    assert!(matches!(fetch(0, None), Err(SourceError::NotFound(_))));
    fixture.refuse_content(file.local_item_id, None);
    assert!(fetch(0, None).is_ok());
    source.forget(&id);
    assert_eq!(fixture.fixture_content_source().list_forgotten(), [id.clone()]);
    assert_eq!(fixture.fixture_content_source().list_fetches()[1], ContentFetch { onedrive_item_id: id, from: 3, end: Some(6) });
    assert_eq!(fixture.fixture_content_source().list_fetches().len(), 7);
}

#[test]
fn the_clock_the_poll_handle_and_the_take_downs_are_the_tests_to_hold() {
    let fixture = ItemFixture::new();
    let clock = fixture.clock();
    assert_eq!(clock(), START_TIME);
    fixture.now.store(START_TIME + 60, Ordering::SeqCst);
    assert_eq!(clock(), START_TIME + 60);

    let (_poll, asked) = fixture.poll_handle();
    assert_eq!(asked.load(Ordering::SeqCst), 0);

    assert!(fixture.access_mode_switch.is_read_write(), "the fixture's own folder takes changes from the start");
    let cause = TakeDownCause::StoreFailed { detail: "a test".into() };
    fixture.mount_take_down.ask(cause.clone());
    assert_eq!(*crate::panic::lock(&fixture.take_downs), [cause]);
    assert!(!fixture.access_mode_switch.is_read_write());
}

/// Runs `step` through the fixture's stand-in.
fn run_stand_in_step(fixture: &ItemFixture, step: SyncStep) -> StepOutcome {
    let runners = fixture.step_runners_stand_in();
    fixture.runtime().block_on((runners.run_step)(step))
}

const WRITTEN: StepOutcome = StepOutcome::Written { events: Vec::new() };

#[test]
fn the_stand_in_shows_the_items_of_one_plan_together_each_answered_by_itself() {
    let fixture = ItemFixture::new();
    let directory = fixture.list_onedrive_directory("docs");
    let file = fixture.list_onedrive_file("docs/a.txt", b"a");
    let stale = fixture.versions_of(file.local_item_id);
    fixture.list_onedrive_file("docs/a.txt", b"another");
    let other = fixture.list_onedrive_file("b.txt", b"b");
    let runners = fixture.step_runners_stand_in();

    let outcomes = fixture.runtime().block_on((runners.run_show_steps)(vec![fixture.versions_of(directory.local_item_id), stale, fixture.versions_of(other.local_item_id)]));

    assert_eq!(outcomes, [WRITTEN, StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing), WRITTEN], "a step planned from versions that are no more changes nothing");
    assert!(fixture.find_item_at("docs").is_some() && fixture.find_item_at("b.txt").is_some() && fixture.find_item_at("docs/a.txt").is_none());
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn the_stand_in_commits_a_step_towards_onedrive_with_a_new_id_a_fresh_etag_and_the_content_sent() {
    let fixture = ItemFixture::new();
    let own = fixture.make_own_file("own.txt", b"own");
    let versions = fixture.versions_of(own.local_item_id);
    let step = SyncStep::OneDrive(OneDriveStep::SendNewFile { item: versions.clone(), parent: ItemFixture::onedrive_reference_of(""), name: "own.txt".into() });

    assert_eq!(run_stand_in_step(&fixture, step.clone()), WRITTEN);

    let sent = fixture.find_item(own.local_item_id).unwrap();
    let hash = make_stand_in_hash(versions.local.as_ref().unwrap().content.as_ref().unwrap());
    let confirmed = ContentVersion::Hashed { size: 3, mtime: START_TIME, mtime_nsec: 0, ctag: Some(format!("c-{hash}")), hash: Some(hash) };
    assert_eq!(sent.onedrive_item.as_ref().map(|reference| reference.item_id.as_str()), Some("stand-in-1"));
    assert_eq!((sent.onedrive_etag.as_deref(), sent.file_sync_state()), (Some("e-stand-in-1"), Some(FileSyncState::Synced)));
    assert_eq!((sent.synced_content.as_ref(), sent.onedrive.as_ref().unwrap().content.as_ref()), (Some(&confirmed), Some(&confirmed)));
    assert_eq!(run_stand_in_step(&fixture, step), StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing), "the same step again finds other rows than it was planned from");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn the_stand_in_refuses_a_name_another_item_of_onedrive_has_and_writes_nothing() {
    let fixture = ItemFixture::new();
    fixture.list_onedrive_file("Report.txt", b"theirs");
    // The folder does not show OneDrive's file, and a file is made here under a name
    // that is the same to OneDrive.
    let own = fixture.make_own_file("report.txt", b"mine");
    let rows = fixture.list_items();
    let step = SyncStep::OneDrive(OneDriveStep::SendNewFile { item: fixture.versions_of(own.local_item_id), parent: ItemFixture::onedrive_reference_of(""), name: "report.txt".into() });

    let outcome = run_stand_in_step(&fixture, step);

    assert_eq!(outcome, StepOutcome::Refused(StepRefusal::NameTaken { place: own.local.unwrap().place.unwrap(), holder_name: "Report.txt".into() }));
    assert_eq!(fixture.list_items(), rows);
}

#[test]
fn the_stand_in_commits_a_step_towards_the_folder_with_nothing_on_disk() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"a");

    let outcome = run_stand_in_step(&fixture, SyncStep::Folder(FolderStep::DownloadContent { item: fixture.versions_of(file.local_item_id) }));

    assert_eq!(outcome, WRITTEN);
    assert_eq!(fixture.find_item(file.local_item_id).unwrap().content_presence, ContentPresence::OnDisk);
    assert_eq!(fixture.read_disk_file("a.txt"), None, "the stand-in touches no disk");
}

#[tokio::test]
async fn the_fake_onedrive_answers_the_drive_the_fixtures_store_knows() {
    let fixture = ItemFixture::new();
    let fake_onedrive = fixture.start_fake_onedrive().await;

    assert_eq!(fake_onedrive.client().drive_id().await.expect("the drive"), DRIVE_ID);
    // The quota is the fake's own, and its reads are counted as the fake counts them.
    fake_onedrive.with(|cloud| cloud.free = Some(7));
    assert_eq!(fake_onedrive.client().quota().await.expect("the quota").remaining, Some(7));
    assert_eq!(fake_onedrive.with(|cloud| cloud.quota_reads()), 2);
}

#[tokio::test]
async fn the_stores_delta_link_is_the_fake_onedrives_and_a_listing_the_fixture_writes_leaves_it() {
    let fixture = ItemFixture::new();
    fixture.list_onedrive_file("before.txt", b"listed before the fake began");
    let fake_onedrive = fixture.start_fake_onedrive().await;

    let link = fixture.write_rows(|store| store.delta_link()).expect("a delta link");
    assert_eq!(Some(link.as_str()), fake_onedrive.with(|cloud| cloud.delta_body())["@odata.deltaLink"].as_str());
    assert_eq!(fixture.read_rows(|reader| reader.find_listing_under_way()), None);
    // Asked from it, OneDrive says nothing changed: the fake holds what the rows say.
    let konedrive_graph::drive::DeltaPage { items, .. } = fake_onedrive.client().delta(&konedrive_graph::drive::DeltaFrom::Link(link.clone())).await.expect("a page");
    assert!(items.is_empty());

    fixture.list_onedrive_file("after.txt", b"listed after");
    fixture.list_onedrive_directory("after");
    fixture.list_onedrive_move("after.txt", "after/moved.txt");
    fixture.list_onedrive_removal("before.txt");
    assert_eq!(fixture.write_rows(|store| store.delta_link()), Some(link));
}
