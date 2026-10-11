use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use konedrive_tree::{make_temporary_name, Change, Kind, ListingLink, OneDriveItemReference, Placement, Row};

use super::*;
use crate::folder::running_steps::{RunningStep, StepClass};
use crate::fuse::item_download_landing::ItemDownloadLanding;
use crate::fuse::item_fixture::{DiskMarksRead, ItemFixture, ONEDRIVE_MTIME, ROOT_ONEDRIVE_ID};
use crate::status::report::Report;

/// The races' test of design §12: the steps against a program's writes and renames.
mod races;

pub(super) fn make_steps(fixture: &ItemFixture) -> Arc<FolderSteps> {
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
    Arc::new(FolderSteps::new(
        Arc::clone(&fixture.place_changes),
        fixture.store.clone(),
        Arc::clone(&fixture.write_operation_thread),
        Arc::clone(&fixture.upload_rules),
        Arc::clone(&fixture.item_handles),
        Arc::clone(&fixture.inotify_bridge),
        downloader,
        Arc::clone(&fixture.download_states),
        Arc::clone(&fixture.range_fetcher),
        Arc::clone(&fixture.mount_take_down),
        fixture.folder(),
    ))
}

pub(super) fn run(fixture: &ItemFixture, steps: &Arc<FolderSteps>, step: FolderStep) -> StepOutcome {
    if let FolderStep::DownloadContent { item } = &step {
        // The plan's task says the step runs before it starts it.
        fixture.running_steps.end(item.local_item_id);
        assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Download, item: item.local_item_id, other_items: Vec::new(), destination: None, sent: 0, total: 0 }));
    }
    fixture.runtime().block_on(steps.run(step))
}

/// Everything in the backing directory, by path: a file's bytes, `None` for a directory,
/// and the item id each is marked with.
pub(super) fn read_disk(fixture: &ItemFixture) -> BTreeMap<String, (Option<Vec<u8>>, Option<String>)> {
    fn read_below(fixture: &ItemFixture, directory: &Path, found: &mut BTreeMap<String, (Option<Vec<u8>>, Option<String>)>) {
        for entry in fs::read_dir(fixture.folder().join(directory)).expect("the directory is read") {
            let path = directory.join(entry.expect("an entry").file_name());
            let (name, on_disk) = (path.to_string_lossy().into_owned(), fixture.folder().join(&path));
            let mark = fixture.read_disk_marks(&name).onedrive_item_id;
            if on_disk.is_dir() {
                found.insert(name, (None, mark));
                read_below(fixture, &path, found);
            } else {
                found.insert(name, (Some(fs::read(&on_disk).expect("the file")), mark));
            }
        }
    }
    let mut found = BTreeMap::new();
    read_below(fixture, Path::new(""), &mut found);
    found
}

pub(super) fn count_records(fixture: &ItemFixture) -> usize {
    fixture.write_rows(|store| store.item_store().list_place_change_records()).len()
}

fn make_clean_copy(fixture: &ItemFixture, path: &str, content: &[u8]) -> Item {
    let shown = fixture.show_onedrive_file(path, content);
    fixture.land_copy(shown.local_item_id, content);
    fixture.find_item(shown.local_item_id).expect("the row")
}

/// The cases that change a name on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    MoveItem,
    TakeMoveBack,
    MoveToTemporaryName,
    RenameToConflictCopy,
    RemoveItem,
    TakeOneDriveContent,
}

const CASES: [Case; 6] = [Case::MoveItem, Case::TakeMoveBack, Case::MoveToTemporaryName, Case::RenameToConflictCopy, Case::RemoveItem, Case::TakeOneDriveContent];

/// A folder in which the step of `case` is to run for the file at `a.txt`, whose content
/// is on disk: the step, as a plan made now carries it.
fn make_case(fixture: &ItemFixture, case: Case) -> FolderStep {
    fixture.show_onedrive_directory("docs");
    let file = match case {
        Case::RenameToConflictCopy => fixture.make_own_file("a.txt", b"the content"),
        _ => make_clean_copy(fixture, "a.txt", b"the content"),
    };
    let id = file.local_item_id;
    match case {
        Case::MoveItem => {
            fixture.list_onedrive_move("a.txt", "docs/b.txt");
            FolderStep::MoveItem { item: fixture.versions_of(id) }
        }
        Case::TakeMoveBack => {
            fixture.move_here(id, "docs/moved.txt");
            FolderStep::TakeMoveBack { item: fixture.versions_of(id) }
        }
        Case::MoveToTemporaryName => {
            fixture.list_onedrive_move("a.txt", "docs/b.txt");
            let place = ItemPlace { parent_local_item_id: fixture.find_item_at("docs").unwrap().local_item_id, name: make_temporary_name(id, 1) };
            FolderStep::MoveToTemporaryName { item: fixture.versions_of(id), place }
        }
        Case::RenameToConflictCopy => FolderStep::RenameToConflictCopy { item: fixture.versions_of(id), name: "a (conflict).txt".into() },
        Case::RemoveItem => {
            fixture.list_onedrive_removal("a.txt");
            FolderStep::RemoveItem { item: fixture.versions_of(id) }
        }
        Case::TakeOneDriveContent => {
            fixture.list_onedrive_file("a.txt", b"OneDrive's new content");
            FolderStep::TakeOneDriveContent { item: fixture.versions_of(id) }
        }
    }
}

/// Where the file of `case` is after its step, when it is anywhere in the folder.
fn path_after(case: Case, item: LocalItemId) -> Option<String> {
    match case {
        Case::MoveItem => Some("docs/b.txt".into()),
        Case::TakeMoveBack => Some("a.txt".into()),
        Case::MoveToTemporaryName => Some(format!("docs/.konedrive-swap-{item}")),
        Case::RenameToConflictCopy => Some("a (conflict).txt".into()),
        Case::RemoveItem => None,
        Case::TakeOneDriveContent => Some("a.txt".into()),
    }
}

/// The step in its two halves, as one job of the write-operation thread, with `between`
/// run after the disk was changed and before the commit; `None` from `between` is a death
/// there: nothing more runs, nothing is undone.
fn run_in_halves(fixture: &ItemFixture, steps: &Arc<FolderSteps>, step: &FolderStep, between: impl FnOnce() -> Option<()> + Send + 'static) -> Option<StepOutcome> {
    let (steps, step) = (Arc::clone(steps), step.clone());
    fixture.run_on_write_operation_thread(move || {
        let item = step.item().clone();
        let upload_rules = Arc::clone(&steps.upload_rules);
        let prepared = match &step {
            FolderStep::MoveItem { item } | FolderStep::TakeMoveBack { item } => steps.prepare_move(item, &item.onedrive.clone().unwrap().place.unwrap()),
            FolderStep::MoveToTemporaryName { item, place } => steps.prepare_move(item, place),
            FolderStep::RenameToConflictCopy { item, name } => steps.prepare_move(item, &ItemPlace { parent_local_item_id: item.local.clone().unwrap().place.unwrap().parent_local_item_id, name: name.clone() }),
            FolderStep::RemoveItem { item } => steps.prepare_set_aside(item, true),
            FolderStep::TakeOneDriveContent { item } => steps.prepare_set_aside(item, false),
            FolderStep::ShowItem { .. } | FolderStep::DownloadContent { .. } => panic!("a step that changes no name has no halves"),
        };
        let (change, disk_change) = match prepared {
            Ok(prepared) => prepared,
            Err(outcome) => return Some(outcome),
        };
        between()?;
        let commit: StepCommit = match step {
            FolderStep::MoveItem { .. } => Box::new(move |store, record| store.item_store().commit_move_item(&item, false, record)),
            FolderStep::TakeMoveBack { .. } => Box::new(move |store, record| store.item_store().commit_take_move_back(&item, false, record)),
            FolderStep::MoveToTemporaryName { place, .. } => Box::new(move |store, record| store.item_store().commit_move_to_temporary_name(&item, &place, false, record)),
            FolderStep::RenameToConflictCopy { name, .. } => {
                let conflict = ConflictRow { at: 0, original: "a.txt".into(), rescued: "the copy".into(), kind: ConflictKind::Copy };
                Box::new(move |store, record| store.item_store().commit_rename_to_conflict_copy_on_record(&item, &name, upload_rules.is_name_ignored(&name), &conflict, record))
            }
            FolderStep::RemoveItem { .. } => Box::new(move |store, record| store.item_store().commit_remove_item(&item, record)),
            FolderStep::TakeOneDriveContent { .. } => Box::new(move |store, record| store.item_store().commit_take_onedrive_content(&item, record)),
            FolderStep::ShowItem { .. } | FolderStep::DownloadContent { .. } => unreachable!(),
        };
        Some(match steps.commit_on_record(change, disk_change, commit) {
            Ok(_) => StepOutcome::Written { events: Vec::new() },
            Err(outcome) => outcome,
        })
    })
}

/// What a listing's page does to the row of `item` between a step's disk and its commit:
/// OneDrive says the item again, with another eTag. Made here, to be written inside the
/// step's job.
fn make_crossing_page(fixture: &ItemFixture, item: LocalItemId) -> impl FnOnce() -> Option<()> + Send + 'static {
    let row = fixture.find_item(item).expect("the row");
    let binding = row.onedrive_item.clone().unwrap_or_else(|| OneDriveItemReference { drive_id: fixture.drive_id(), item_id: konedrive_tree::OneDriveItemId::new("never-listed") });
    let store = fixture.store.clone();
    let listed = Row { id: binding.item_id.as_str().into(), parent_id: Some(ROOT_ONEDRIVE_ID.into()), name: "listed again.txt".into(), kind: Kind::File, size: 3, mtime: ONEDRIVE_MTIME, etag: Some("e-crossing".into()), ctag: Some("c-crossing".into()), quickxor: None, mime: None, placement: Placement::Placed };
    let is_bound = row.onedrive_item.is_some();
    move || {
        store
            .call_blocking(move |store| {
                if !is_bound {
                    // An item OneDrive does not have is crossed by a program's own change.
                    return store.item_store().apply(&konedrive_tree::LocalVersionChange::OpenedForWriting { item }).map(|_| ());
                }
                let began = store.item_store().begin_delta_listing()?;
                store.item_store().write_listed_changes(began.id, &[Change::Upsert(listed)], began.began_at_count, &ListingLink::DeltaLink("link-crossing".into())).map(|_| ())
            })
            .expect("the rows are crossed");
        Some(())
    }
}

#[test]
fn the_obstacle_of_a_step_is_decided_from_the_row_and_its_writers() {
    let fixture = ItemFixture::new();
    let row = make_clean_copy(&fixture, "a.txt", b"the content");
    let planned = ItemVersions::of(&row);

    assert_eq!(decide_folder_step_obstacle(&planned, Some(&row), false), None);
    assert_eq!(decide_folder_step_obstacle(&planned, Some(&row), true), Some(StepOutcome::NotRun(StepObstacle::OpenForWriting)));
    assert_eq!(decide_folder_step_obstacle(&planned, None, false), Some(rows_changed()), "the row went");
    let listed_again = Item { onedrive_etag: Some("e-later".into()), ..row.clone() };
    assert_eq!(decide_folder_step_obstacle(&planned, Some(&listed_again), false), Some(rows_changed()));
    assert_eq!(decide_folder_step_obstacle(&planned, Some(&listed_again), true), Some(rows_changed()), "other versions before a writer: the next plan decides");
}

#[test]
fn what_a_download_answered_is_the_steps_outcome_case_by_case() {
    let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a.txt".into() };
    let wait = std::time::Duration::from_secs(7);
    assert_eq!(decide_download_outcome(Ok(ItemLandingOutcome::Landed { place, path: "a.txt".into() })), StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Downloaded, path: "a.txt".into(), former_path: None }] });
    assert_eq!(decide_download_outcome(Ok(ItemLandingOutcome::RowsChanged)), rows_changed());
    assert_eq!(decide_download_outcome(Ok(ItemLandingOutcome::PlaceHeld { path: "a.txt".into() })), StepOutcome::NotRun(StepObstacle::PlaceHeldOnDisk { path: "a.txt".into() }));
    assert_eq!(decide_download_outcome(Ok(ItemLandingOutcome::Failed { errno: libc::ENOSPC })), StepOutcome::Failed(StepFailure::Disk { errno: libc::ENOSPC, path: PathBuf::new() }));
    assert_eq!(decide_download_outcome(Ok(ItemLandingOutcome::StepStopped)), StepOutcome::NotRun(StepObstacle::SendingStopped));
    assert_eq!(decide_download_outcome(Err(DownloadRefusal::SignedOut)), StepOutcome::Refused(StepRefusal::SignedOut));
    assert_eq!(decide_download_outcome(Err(DownloadRefusal::Throttled { retry_after: Some(wait) })), StepOutcome::Refused(StepRefusal::Throttled { retry_after: Some(wait) }));
    assert_eq!(decide_download_outcome(Err(DownloadRefusal::ItemGone)), StepOutcome::Refused(StepRefusal::ItemGone));
    assert_eq!(decide_download_outcome(Err(DownloadRefusal::Transient { detail: "no network".into() })), StepOutcome::Refused(StepRefusal::Transient { detail: "no network".into() }));
}

#[test]
fn each_case_changes_the_disk_and_the_rows_as_its_line_of_the_table_says() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
        let former_path = fixture.read_rows(move |reader| reader.find_path(id)).expect("the item is shown");

        let outcome = run(&fixture, &steps, step.clone());

        let StepOutcome::Written { events } = &outcome else { panic!("{case:?}: the step is written: {outcome:?}") };
        assert_eq!(events.len(), 1, "{case:?}");
        let (row, disk) = (fixture.find_item(id).expect("the row stays"), read_disk(&fixture));
        let path_now = fixture.read_rows(move |reader| reader.find_path(id)).map(|path| path.to_string_lossy().into_owned());
        assert_eq!(path_now, path_after(case, id), "{case:?}: where the folder shows it");
        match case {
            Case::RemoveItem => {
                assert_eq!(row.local, None, "{case:?}");
                assert!(!disk.contains_key("a.txt"), "{case:?}: its copy went with it");
                assert_eq!(events[0], StepEvent { kind: ActivityKind::Removed, path: "a.txt".into(), former_path: None });
            }
            Case::TakeOneDriveContent => {
                assert_eq!(row.content_presence, ContentPresence::NotOnDisk);
                assert_eq!(row.local.as_ref().unwrap().content, Some(ItemFixture::onedrive_content_of(b"OneDrive's new content")));
                assert!(!disk.contains_key("a.txt"), "{case:?}: the copy of the old content is gone");
                assert_eq!(fixture.fixture_content_source().list_forgotten(), ["a.txt"], "what the source remembered of the item is dropped");
            }
            _ => {
                let to = path_after(case, id).unwrap();
                assert_eq!(disk.get(&to).and_then(|(content, _)| content.as_deref()), Some(&b"the content"[..]), "{case:?}: the file is at its new place");
                assert!(!disk.contains_key(&former_path.to_string_lossy().into_owned()), "{case:?}: and not at its former one");
                assert_eq!(events[0].former_path.as_deref(), Some(former_path.as_path()), "{case:?}");
                assert_eq!(events[0].path, PathBuf::from(&to), "{case:?}");
            }
        }
        if case == Case::MoveItem || case == Case::MoveToTemporaryName {
            assert_eq!(fixture.read_disk_marks("docs"), DiskMarksRead { onedrive_item_id: Some("docs".into()), is_whole: false, ctag: None }, "{case:?}: the directory above was made and marked");
        }
        assert!(disk.keys().all(|path| !path.starts_with(".konedrive-aside")), "{case:?}: nothing is left set aside: {disk:?}");
        assert_eq!(count_records(&fixture), 0, "{case:?}: the commit took the record");
        fixture.assert_rows_and_disk_agree();
    }
}

#[test]
fn a_conflict_copy_is_on_record_with_its_commit() {
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::RenameToConflictCopy);
    let steps = make_steps(&fixture);

    let outcome = run(&fixture, &steps, step);

    assert_eq!(outcome, StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Conflict, path: "a (conflict).txt".into(), former_path: Some("a.txt".into()) }] });
    let conflicts = fixture.write_rows(|store| store.conflicts());
    assert_eq!(conflicts.len(), 1);
    assert_eq!((conflicts[0].original.clone(), conflicts[0].rescued.clone(), conflicts[0].kind), (fixture.disk_path("a.txt").display().to_string(), fixture.disk_path("a (conflict).txt").display().to_string(), ConflictKind::Copy), "the two full paths");
}

#[test]
fn a_step_planned_from_other_versions_than_the_rows_touches_nothing() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let outdated = |item: &ItemVersions| ItemVersions { onedrive_etag: Some("an eTag the row never had".into()), ..item.clone() };
        let step = match step {
            FolderStep::MoveItem { item } => FolderStep::MoveItem { item: outdated(&item) },
            FolderStep::TakeMoveBack { item } => FolderStep::TakeMoveBack { item: outdated(&item) },
            FolderStep::MoveToTemporaryName { item, place } => FolderStep::MoveToTemporaryName { item: outdated(&item), place },
            FolderStep::RenameToConflictCopy { item, name } => FolderStep::RenameToConflictCopy { item: outdated(&item), name },
            FolderStep::RemoveItem { item } => FolderStep::RemoveItem { item: outdated(&item) },
            FolderStep::TakeOneDriveContent { item } => FolderStep::TakeOneDriveContent { item: outdated(&item) },
            step => step,
        };
        let (steps, disk_before, rows_before) = (make_steps(&fixture), read_disk(&fixture), fixture.list_items());

        assert_eq!(run(&fixture, &steps, step), rows_changed(), "{case:?}");

        assert_eq!(read_disk(&fixture), disk_before, "{case:?}: nothing on disk changed");
        assert_eq!(fixture.list_items(), rows_before, "{case:?}");
        assert_eq!(count_records(&fixture), 0, "{case:?}: no record was written");
    }
}

#[test]
fn a_step_for_a_file_a_program_opened_for_writing_since_touches_nothing() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
        let path = fixture.read_rows(move |reader| reader.find_path(id)).unwrap();
        let (writer, _) = fixture.item_handles.add_writer(id, Arc::new(fs::File::open(fixture.folder().join(path)).unwrap()));
        let disk_before = read_disk(&fixture);

        assert_eq!(run(&fixture, &steps, step.clone()), StepOutcome::NotRun(StepObstacle::OpenForWriting), "{case:?}");
        assert_eq!(read_disk(&fixture), disk_before, "{case:?}");
        assert_eq!(count_records(&fixture), 0, "{case:?}");

        fixture.item_handles.remove_writer(writer);
        assert!(matches!(run(&fixture, &steps, step), StepOutcome::Written { .. }), "{case:?}: after the close the same step runs");
    }
}

#[test]
fn a_commit_that_finds_the_rows_changed_undoes_the_disk() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
        let disk_before = read_disk(&fixture);
        let (seen, crossing) = (Arc::new(std::sync::Mutex::new(None)), make_crossing_page(&fixture, id));
        let (seen_between, store, folder) = (Arc::clone(&seen), fixture.store.clone(), fixture.folder());

        let outcome = run_in_halves(&fixture, &steps, &step, move || {
            let records = store.call_blocking(|store| store.item_store().list_place_change_records()).expect("the records");
            *seen_between.lock().unwrap() = Some((records.len(), folder.join("a.txt").exists(), folder.join("docs/moved.txt").exists()));
            crossing()
        });

        assert_eq!(outcome, Some(rows_changed()), "{case:?}");
        let (records_between, is_at_first_place, is_at_moved_place) = seen.lock().unwrap().take().expect("the step got to its disk change");
        assert_eq!(records_between, 1, "{case:?}: the record stood from before the disk until the undo");
        assert!(!is_at_first_place && (case != Case::TakeMoveBack || !is_at_moved_place) || case == Case::TakeMoveBack && !is_at_moved_place, "{case:?}: the disk was changed before the commit");
        assert_eq!(read_disk(&fixture), disk_before, "{case:?}: the disk is as it was");
        assert_eq!(count_records(&fixture), 0, "{case:?}: the record the commit left was taken by itself");
        assert!(fixture.take_downs.lock().unwrap().is_empty(), "{case:?}");
        fixture.assert_rows_and_disk_agree();
    }
}

#[test]
fn a_removed_file_is_put_back_when_its_commit_is_refused() {
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::RemoveItem);
    let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
    let marks_before = fixture.read_disk_marks("a.txt");

    let outcome = run_in_halves(&fixture, &steps, &step, make_crossing_page(&fixture, id));

    assert_eq!(outcome, Some(rows_changed()));
    assert_eq!(fixture.read_disk_file("a.txt").as_deref(), Some(&b"the content"[..]), "back under its name");
    assert_eq!(fixture.read_disk_marks("a.txt"), marks_before, "with its marks");
    assert!(fixture.find_item(id).unwrap().local.is_some(), "the folder still shows it");
}

#[test]
fn an_undo_that_fails_asks_for_the_mounts_take_down() {
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::MoveItem);
    let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
    let (crossing, folder) = (make_crossing_page(&fixture, id), fixture.folder());

    let outcome = run_in_halves(&fixture, &steps, &step, move || {
        // What no row says takes the former name: the move back replaces nothing.
        fs::write(folder.join("a.txt"), b"in the way").expect("a file by hand");
        crossing()
    });

    assert_eq!(outcome, Some(StepOutcome::Failed(StepFailure::UndoFailed { path: "docs/b.txt".into() })));
    assert_eq!(*fixture.take_downs.lock().unwrap(), [TakeDownCause::UndoFailed { path: "docs/b.txt".into() }]);
    assert_eq!(count_records(&fixture), 1, "the record stands for the check at start");
    assert_eq!(fixture.read_disk_file("docs/b.txt").as_deref(), Some(&b"the content"[..]));

    // From here on no step touches the folder.
    let other = make_clean_copy(&fixture, "other.txt", b"other");
    fixture.list_onedrive_removal("other.txt");
    assert_eq!(run(&fixture, &steps, FolderStep::RemoveItem { item: fixture.versions_of(other.local_item_id) }), StepOutcome::NotRun(StepObstacle::SendingStopped));
    assert_eq!(fixture.read_disk_file("other.txt").as_deref(), Some(&b"other"[..]));
}

#[test]
fn a_step_after_a_take_down_was_asked_for_and_one_whose_thread_stopped_do_not_run() {
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::RemoveItem);
    let listed = fixture.list_onedrive_file("new.txt", b"new");
    let pinned = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let steps = make_steps(&fixture);
    let all = [step, FolderStep::ShowItem { item: ItemVersions::of(&listed) }, FolderStep::DownloadContent { item: ItemVersions::of(&pinned) }];
    let (disk_before, rows_before) = (read_disk(&fixture), fixture.list_items());

    let take_down = Arc::clone(&fixture.mount_take_down);
    fixture.run_on_write_operation_thread(move || take_down.ask(TakeDownCause::StoreFailed { detail: "the test asks".into() }));
    for step in all.clone() {
        assert_eq!(run(&fixture, &steps, step.clone()), StepOutcome::NotRun(StepObstacle::SendingStopped), "{step:?}");
    }
    assert_eq!((read_disk(&fixture), fixture.list_items()), (disk_before, rows_before));

    let stopped = ItemFixture::new();
    let step = make_case(&stopped, Case::RemoveItem);
    let steps = make_steps(&stopped);
    stopped.write_operation_thread.stop();
    assert_eq!(run(&stopped, &steps, step.clone()), StepOutcome::NotRun(StepObstacle::SendingStopped), "a job dropped because the thread stopped");
    assert_eq!(stopped.runtime().block_on(steps.run_show_items(vec![step.item().clone(), step.item().clone()])), vec![StepOutcome::NotRun(StepObstacle::SendingStopped); 2]);
}

#[test]
fn an_item_with_nothing_on_disk_is_moved_in_its_row_alone() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("a.txt", b"the content");
    let directory = fixture.show_onedrive_directory("photos");
    fixture.list_onedrive_move("a.txt", "docs/b.txt");
    fixture.list_onedrive_move("photos", "docs/pictures");
    let steps = make_steps(&fixture);
    let (store, seen) = (fixture.store.clone(), Arc::new(std::sync::Mutex::new(None)));
    let seen_between = Arc::clone(&seen);

    let moved = run_in_halves(&fixture, &steps, &FolderStep::MoveItem { item: fixture.versions_of(file.local_item_id) }, move || {
        *seen_between.lock().unwrap() = Some(store.call_blocking(|store| store.item_store().list_place_change_records()).expect("the records").len());
        Some(())
    });
    assert!(matches!(moved, Some(StepOutcome::Written { .. })));
    assert_eq!(*seen.lock().unwrap(), Some(0), "no record for a step that changes no name on disk");
    assert!(matches!(run(&fixture, &steps, FolderStep::MoveItem { item: fixture.versions_of(directory.local_item_id) }), StepOutcome::Written { .. }));

    assert_eq!(read_disk(&fixture), BTreeMap::new(), "nothing is made on disk");
    assert_eq!(fixture.find_item_at("docs/b.txt").map(|row| row.local_item_id), Some(file.local_item_id));
    assert_eq!(fixture.find_item_at("docs/pictures").map(|row| row.local_item_id), Some(directory.local_item_id));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_the_row_says_is_not_on_disk_leaves_what_no_row_says_at_its_path() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"the content");
    fixture.write_disk_file("a.txt", b"what no row says");
    fixture.list_onedrive_removal("a.txt");
    let steps = make_steps(&fixture);

    assert!(matches!(run(&fixture, &steps, FolderStep::RemoveItem { item: fixture.versions_of(file.local_item_id) }), StepOutcome::Written { .. }));

    assert_eq!(fixture.read_disk_file("a.txt").as_deref(), Some(&b"what no row says"[..]), "no step removes what is not its item's");
    assert_eq!(fixture.find_item(file.local_item_id).unwrap().local, None);
}

#[test]
fn a_moved_directory_gets_its_directories_above_made_and_marked() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("x");
    fixture.show_onedrive_directory("x/y");
    let directory = fixture.show_onedrive_directory("photos");
    let inside = make_clean_copy(&fixture, "photos/one.jpg", b"a picture");
    fixture.list_onedrive_move("photos", "x/y/pictures");
    let steps = make_steps(&fixture);

    let outcome = run(&fixture, &steps, FolderStep::MoveItem { item: fixture.versions_of(directory.local_item_id) });

    assert_eq!(outcome, StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Moved, path: "x/y/pictures".into(), former_path: Some("photos".into()) }] });
    let disk = read_disk(&fixture);
    assert_eq!(disk.get("x"), Some(&(None, Some("x".into()))), "made and marked");
    assert_eq!(disk.get("x/y"), Some(&(None, Some("x!y".into()))));
    assert_eq!(disk.get("x/y/pictures"), Some(&(None, Some("photos".into()))), "the directory keeps its own mark");
    assert_eq!(disk.get("x/y/pictures/one.jpg").and_then(|(content, _)| content.as_deref()), Some(&b"a picture"[..]), "what is in it went with it");
    assert!(!disk.contains_key("photos"));
    assert_eq!(fixture.find_item_at("x/y/pictures/one.jpg").map(|row| row.local_item_id), Some(inside.local_item_id));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_place_held_on_disk_by_what_no_row_says_stops_the_step_and_is_said() {
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::MoveItem);
    let steps = make_steps(&fixture);
    fs::create_dir(fixture.disk_path("docs")).unwrap();
    fixture.write_disk_file("docs/b.txt", b"what no row says");
    let (disk_before, rows_before) = (read_disk(&fixture), fixture.list_items());

    let outcome = run(&fixture, &steps, step);

    assert_eq!(outcome, StepOutcome::NotRun(StepObstacle::PlaceHeldOnDisk { path: "docs/b.txt".into() }));
    let mut disk = read_disk(&fixture);
    // The directory that was on disk unmarked is marked, which is not undone.
    disk.insert("docs".into(), (None, None));
    assert_eq!(disk, disk_before, "nothing is replaced and nothing moved");
    assert_eq!(fixture.list_items(), rows_before);
    assert_eq!(count_records(&fixture), 0, "the record of a change that was not made is taken");
}

#[test]
fn a_directory_that_leaves_is_removed_on_disk_only_when_it_is_empty_there() {
    let fixture = ItemFixture::new();
    let (empty, held, shown_in) = (fixture.show_onedrive_directory("empty"), fixture.show_onedrive_directory("held"), fixture.show_onedrive_directory("shown in"));
    for directory in [&empty, &held, &shown_in] {
        fixture.make_directory_on_disk(directory.local_item_id);
    }
    fixture.write_disk_file("held/what no row says.txt", b"the person's");
    make_clean_copy(&fixture, "shown in/a.txt", b"a");
    for path in ["empty", "held", "shown in"] {
        fixture.list_onedrive_removal(path);
    }
    let steps = make_steps(&fixture);
    let remove = |directory: &Item| run(&fixture, &steps, FolderStep::RemoveItem { item: fixture.versions_of(directory.local_item_id) });

    assert!(matches!(remove(&empty), StepOutcome::Written { .. }));
    assert!(matches!(remove(&held), StepOutcome::Written { .. }));
    assert_eq!(remove(&shown_in), rows_changed(), "the folder shows something in it");

    let disk = read_disk(&fixture);
    assert!(!disk.contains_key("empty"), "an empty directory goes");
    assert_eq!(disk.get("held"), Some(&(None, None)), "one that holds what no row says stays, its mark taken off");
    assert_eq!(disk.get("held/what no row says.txt").and_then(|(content, _)| content.as_deref()), Some(&b"the person's"[..]));
    assert_eq!(disk.get("shown in"), Some(&(None, Some("shown in".into()))), "one the folder shows something in is not touched");
    assert_eq!(count_records(&fixture), 0);
}

#[test]
fn the_items_of_several_show_steps_are_committed_in_one_job_each_by_its_own_versions() {
    let fixture = ItemFixture::new();
    let directory = fixture.list_onedrive_directory("docs");
    let inside = fixture.list_onedrive_file("docs/a.txt", b"a");
    let deep = fixture.list_onedrive_directory("docs/deep");
    let ignored = fixture.list_onedrive_file("notes.tmp", b"notes");
    let outdated = fixture.list_onedrive_file("outdated.txt", b"outdated");
    let outdated_versions = ItemVersions { onedrive_etag: Some("an eTag the row never had".into()), ..ItemVersions::of(&outdated) };
    let steps = make_steps(&fixture);
    // What is in a directory is given before the directory.
    let items = vec![ItemVersions::of(&inside), outdated_versions, ItemVersions::of(&deep), ItemVersions::of(&directory), ItemVersions::of(&ignored)];
    assert_eq!(order_shown_items(&items), [1, 3, 4, 0, 2], "a directory before what is in it, and else as given");

    let outcomes = fixture.runtime().block_on(steps.run_show_items(items));

    let added = |path: &str| StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Added, path: path.into(), former_path: None }] };
    assert_eq!(outcomes, [added("docs/a.txt"), rows_changed(), added("docs/deep"), added("docs"), added("notes.tmp")], "each answered by itself, in the order given");
    assert_eq!(fixture.find_item_at("docs/a.txt").map(|row| (row.local_item_id, row.content_presence)), Some((inside.local_item_id, ContentPresence::NotOnDisk)));
    assert!(fixture.find_item_at("notes.tmp").unwrap().is_name_ignored, "each with whether its name is ignored");
    assert_eq!(fixture.find_item(outdated.local_item_id).unwrap().local, None);
    assert_eq!(read_disk(&fixture), BTreeMap::new(), "nothing on disk");

    // One step alone goes the same way.
    let single = fixture.list_onedrive_file("single.txt", b"single");
    assert_eq!(run(&fixture, &steps, FolderStep::ShowItem { item: ItemVersions::of(&single) }), added("single.txt"));
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_pinned_files_content_is_downloaded_as_the_plans_step() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let pinned = fixture.show_onedrive_file("docs/pinned.txt", b"pinned");
    let steps = make_steps(&fixture);

    let outcome = run(&fixture, &steps, FolderStep::DownloadContent { item: ItemVersions::of(&pinned) });

    assert_eq!(outcome, StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Downloaded, path: "docs/pinned.txt".into(), former_path: None }] });
    assert_eq!(fixture.read_disk_file("docs/pinned.txt").as_deref(), Some(&b"pinned"[..]));
    assert_eq!(count_records(&fixture), 0);
    fixture.assert_rows_and_disk_agree();

    // Planned from other versions: nothing lands.
    let other = fixture.show_onedrive_file("other.txt", b"other");
    let outdated = ItemVersions { onedrive_etag: Some("an eTag the row never had".into()), ..ItemVersions::of(&other) };
    assert_eq!(run(&fixture, &steps, FolderStep::DownloadContent { item: outdated }), rows_changed());
    assert_eq!(fixture.read_disk_file("other.txt"), None);

    // What the source refuses is the step's refusal.
    fixture.refuse_content(other.local_item_id, Some(crate::fuse::item_fixture::ContentRefusal::NotFound));
    assert_eq!(run(&fixture, &steps, FolderStep::DownloadContent { item: ItemVersions::of(&other) }), StepOutcome::Refused(StepRefusal::ItemGone));
}

/// The rows, without what a death and its check may move and nothing says: nothing.
fn read_state(fixture: &ItemFixture) -> (Vec<Item>, BTreeMap<String, (Option<Vec<u8>>, Option<String>)>) {
    (fixture.list_items(), read_disk(fixture))
}

#[test]
fn a_death_between_the_disk_and_the_commit_is_undone_by_the_check_at_start_from_the_steps_record() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let steps = make_steps(&fixture);
        let (before, unsent_before) = (read_state(&fixture), fixture.read_rows(|reader| reader.list_unsent(None, 100)));

        assert_eq!(run_in_halves(&fixture, &steps, &step, || None), None, "{case:?}: the daemon dies after the disk");
        assert_eq!(count_records(&fixture), 1, "{case:?}: the record stands");
        assert_ne!(read_disk(&fixture), before.1, "{case:?}: the disk was changed");

        fixture.run_start_check();
        fixture.place_changes.remove_aside_entries().expect("what is still set aside goes");

        let (rows, mut disk) = read_state(&fixture);
        // A directory of the folder that the move made above its place stays, marked: the rows show it.
        if before.1.get("docs").is_none() {
            assert!(matches!(disk.remove("docs"), None | Some((None, Some(_)))), "{case:?}");
        }
        assert_eq!(rows, before.0, "{case:?}: the rows are as they were");
        assert_eq!(disk, before.1, "{case:?}: the disk is put back");
        assert_eq!(count_records(&fixture), 0, "{case:?}");
        assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 100)), unsent_before, "{case:?}: nothing is to send that nobody did");
        fixture.assert_rows_and_disk_agree();

        // And the plan's step, planned again, runs.
        assert!(matches!(run(&fixture, &steps, step), StepOutcome::Written { .. }), "{case:?}");
        fixture.assert_rows_and_disk_agree();
    }
}

#[test]
fn a_death_after_the_record_and_before_the_disk_changes_nothing() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (id, before) = (step.item().local_item_id, read_state(&fixture));
        let place = fixture.find_item(id).unwrap().local.unwrap().place.unwrap();
        // The record alone, as each case writes it.
        let record = match &step {
            FolderStep::RemoveItem { .. } | FolderStep::TakeOneDriveContent { .. } => PlaceChangeRecord::SetAside { item: id, place, aside_name: make_aside_entry_name(id) },
            FolderStep::MoveToTemporaryName { place: to, .. } => PlaceChangeRecord::MoveEntry { item: id, from: place, to: to.clone() },
            FolderStep::RenameToConflictCopy { name, .. } => PlaceChangeRecord::MoveEntry { item: id, from: place.clone(), to: ItemPlace { name: name.clone(), ..place } },
            step => PlaceChangeRecord::MoveEntry { item: id, from: place, to: step.item().onedrive.clone().unwrap().place.unwrap() },
        };
        fixture.write_rows(move |store| store.item_store().record_place_change(&record));

        fixture.run_start_check();

        assert_eq!(read_state(&fixture), before, "{case:?}");
        assert_eq!(count_records(&fixture), 0, "{case:?}");
    }
}

#[test]
fn a_death_after_the_copy_of_a_file_that_takes_onedrives_content_was_set_aside_is_not_read_as_an_edit() {
    // Before the commit: the old row and the record. The old copy comes back under its
    // name, clean, and nothing is to send.
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::TakeOneDriveContent);
    let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
    assert_eq!(run_in_halves(&fixture, &steps, &step, || None), None);
    assert_eq!(fixture.read_disk_file("a.txt"), None, "the copy is set aside");
    fixture.run_start_check();
    assert_eq!(fixture.read_disk_file("a.txt").as_deref(), Some(&b"the content"[..]));
    let row = fixture.find_item(id).unwrap();
    assert_eq!((row.content_presence, row.local.as_ref().unwrap().content.clone()), (ContentPresence::OnDisk, Some(ItemFixture::onedrive_content_of(b"the content"))), "the old content, on disk");
    assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 10)), Vec::new(), "the old copy is no edit");

    // After the commit, before the copy is removed for good: the new row, which says not
    // on disk, and no record. The copy that was set aside is not put back.
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::TakeOneDriveContent);
    let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
    let (store, versions) = (fixture.store.clone(), step.item().clone());
    let dying = Arc::clone(&steps);
    fixture.run_on_write_operation_thread(move || {
        let (change, set_aside) = dying.prepare_set_aside(&versions, false).expect("the copy is set aside");
        let record = set_aside.record;
        let committed = store.call_blocking(move |store| store.item_store().commit_take_onedrive_content(&versions, record)).expect("the commit");
        assert_eq!(committed, StepCommitOutcome::Written);
        // The death: before the change ended and before the discard.
        drop((change, set_aside));
    });
    assert!(read_disk(&fixture).keys().any(|path| path.starts_with(".konedrive-aside")), "the old copy is still set aside");
    fixture.run_start_check();
    fixture.place_changes.remove_aside_entries().expect("what is still set aside goes");
    let row = fixture.find_item(id).unwrap();
    assert_eq!((row.content_presence, row.local.as_ref().unwrap().content.clone()), (ContentPresence::NotOnDisk, Some(ItemFixture::onedrive_content_of(b"OneDrive's new content"))));
    assert_eq!(fixture.read_disk_file("a.txt"), None, "the old copy is not read as the new content, nor as an edit of it");
    assert!(read_disk(&fixture).keys().all(|path| !path.starts_with(".konedrive-aside")));
    assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 10)), Vec::new());
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_commit_that_writes_takes_the_steps_record_and_one_that_finds_the_rows_changed_leaves_it() {
    // What the commit does with the record, seen without the runner's own taking of it.
    let fixture = ItemFixture::new();
    let step = make_case(&fixture, Case::RemoveItem);
    let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
    let (crossing, store, versions) = (make_crossing_page(&fixture, id), fixture.store.clone(), step.item().clone());
    let left = fixture.run_on_write_operation_thread(move || {
        let (change, set_aside) = steps.prepare_set_aside(&versions, true).expect("the file is set aside");
        crossing();
        let record = set_aside.record;
        let committed = store.call_blocking(move |store| store.item_store().commit_remove_item(&versions, record)).expect("the commit");
        let records_left = store.call_blocking(|store| store.item_store().list_place_change_records()).expect("the records").len();
        steps.undo_on_disk(&change, set_aside.undo, record).expect("the file is put back");
        (committed, records_left)
    });
    assert_eq!(left, (StepCommitOutcome::RowsChanged, 1), "a commit that finds the rows changed leaves the record");
    assert_eq!(count_records(&fixture), 0, "the runner takes it after its undo");
    assert_eq!(fixture.read_disk_file("a.txt").as_deref(), Some(&b"the content"[..]));
}

/// What the bridge was told so far, a change after the other: its thread is not started
/// in the fixture, so everything announced is still queued.
fn list_announced(fixture: &ItemFixture) -> Vec<Vec<ItemAnnouncement>> {
    fixture.inotify_bridge.list_queued()
}

fn place_at(fixture: &ItemFixture, directory: &str, name: &str) -> ItemPlace {
    let parent_local_item_id = match directory {
        "" => LocalItemId::ROOT,
        directory => fixture.find_item_at(directory).expect("the directory is shown").local_item_id,
    };
    ItemPlace { parent_local_item_id, name: name.into() }
}

#[test]
fn every_step_announces_the_former_name_and_the_new_one() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
        let former_place = fixture.find_item(id).and_then(|row| row.local).and_then(|local| local.place).expect("the item is shown");
        assert_eq!(list_announced(&fixture), Vec::<Vec<ItemAnnouncement>>::new(), "{case:?}: nothing before the step");

        assert!(matches!(run(&fixture, &steps, step), StepOutcome::Written { .. }), "{case:?}");

        let vanished = ItemAnnouncement::Vanished { item: id, former_place: former_place.clone(), kind: ItemKind::File };
        let appeared = |directory: &str, name: &str| ItemAnnouncement::Appeared { item: id, place: place_at(&fixture, directory, name) };
        let announced = match case {
            Case::MoveItem => vec![vanished, appeared("docs", "b.txt")],
            // Moved here to `docs/moved.txt`, and taken back to where OneDrive has it.
            Case::TakeMoveBack => vec![vanished, appeared("", "a.txt")],
            Case::MoveToTemporaryName => vec![vanished, ItemAnnouncement::Appeared { item: id, place: place_at(&fixture, "docs", &make_temporary_name(id, 1).to_string_lossy()) }],
            Case::RenameToConflictCopy => vec![vanished, appeared("", "a (conflict).txt")],
            Case::RemoveItem => vec![vanished],
            Case::TakeOneDriveContent => vec![ItemAnnouncement::ContentChanged { item: id, place: former_place }],
        };
        assert_eq!(list_announced(&fixture), [announced], "{case:?}: one change, the former name first");
    }

    // The items a plan shows are one change, each appearing at its place, a directory
    // before what is in it.
    let fixture = ItemFixture::new();
    let (directory, inside) = (fixture.list_onedrive_directory("docs"), fixture.list_onedrive_file("docs/a.txt", b"a"));
    let steps = make_steps(&fixture);
    fixture.runtime().block_on(steps.run_show_items(vec![ItemVersions::of(&inside), ItemVersions::of(&directory)]));
    let appeared = |item: &Item, directory: &str, name: &str| ItemAnnouncement::Appeared { item: item.local_item_id, place: place_at(&fixture, directory, name) };
    assert_eq!(list_announced(&fixture), [vec![appeared(&directory, "", "docs"), appeared(&inside, "docs", "a.txt")]]);

    // A download: the file is shown as being downloaded, and then as downloaded.
    let pinned = fixture.show_onedrive_file("docs/pinned.txt", b"pinned");
    let before = list_announced(&fixture).len();
    assert!(matches!(run(&fixture, &steps, FolderStep::DownloadContent { item: ItemVersions::of(&pinned) }), StepOutcome::Written { .. }));
    let changed = vec![ItemAnnouncement::ContentChanged { item: pinned.local_item_id, place: place_at(&fixture, "docs", "pinned.txt") }];
    assert_eq!(list_announced(&fixture)[before..], [changed.clone(), changed]);
}

#[test]
fn a_step_that_writes_nothing_announces_nothing() {
    for case in CASES {
        let fixture = ItemFixture::new();
        let step = make_case(&fixture, case);
        let (steps, id) = (make_steps(&fixture), step.item().local_item_id);
        // A program opened the file for writing since the plan: the step touches nothing.
        fixture.write_rows(move |store| store.item_store().apply(&konedrive_tree::LocalVersionChange::OpenedForWriting { item: id }));

        assert!(!matches!(run(&fixture, &steps, step), StepOutcome::Written { .. }), "{case:?}");

        assert_eq!(list_announced(&fixture), Vec::<Vec<ItemAnnouncement>>::new(), "{case:?}");
    }
}
