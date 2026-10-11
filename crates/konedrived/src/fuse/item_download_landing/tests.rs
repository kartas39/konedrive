use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_tree::{ContentPresence, Item, ItemVersions, LocalVersionChange};

use super::*;
use crate::fake_onedrive::qx;
use crate::folder::running_steps::{RunningStep, StepClass};
use crate::fuse::item_fixture::{DiskMarksRead, ItemFixture, ONEDRIVE_MTIME};

fn make_landing(fixture: &ItemFixture) -> Arc<ItemDownloadLanding> {
    Arc::new(ItemDownloadLanding::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.running_steps)))
}

/// A file with no name that holds `content`, synced: what a download hands its landing.
fn make_downloaded_file(fixture: &ItemFixture, content: &[u8]) -> Arc<File> {
    let mut file = fixture.backing_directory.make_unnamed_file().expect("a file with no name");
    file.write_all(content).expect("the downloaded bytes");
    file.sync_all().expect("the file is synced");
    Arc::new(file)
}

fn land(fixture: &ItemFixture, landing: &Arc<ItemDownloadLanding>, content: &[u8], condition: &LandingCondition) -> ItemLandingOutcome {
    let (landing, file, condition) = (Arc::clone(landing), make_downloaded_file(fixture, content), condition.clone());
    fixture.run_on_write_operation_thread(move || landing.land(&file, &condition))
}

/// The landing in its two halves, with `between` run after the file was linked and before
/// its commit: what a listing's page, which does not take the tree lock, does there.
fn land_with(fixture: &ItemFixture, landing: &Arc<ItemDownloadLanding>, content: &[u8], condition: &LandingCondition, between: impl FnOnce() + Send + 'static) -> ItemLandingOutcome {
    let (landing, file, condition) = (Arc::clone(landing), make_downloaded_file(fixture, content), condition.clone());
    fixture.run_on_write_operation_thread(move || match landing.link_on_record(&file, &condition) {
        Ok((change, linked)) => {
            between();
            landing.commit_linked(&change, linked, &condition)
        }
        Err(outcome) => outcome,
    })
}

fn content_unchanged(row: &Item) -> LandingCondition {
    LandingCondition::ContentUnchanged { item: row.local_item_id, downloaded: row.local.as_ref().and_then(|local| local.content.clone()).expect("a file the folder shows") }
}

fn count_records(fixture: &ItemFixture) -> usize {
    fixture.write_rows(|store| store.item_store().list_place_change_records()).len()
}

fn disk_mtime(fixture: &ItemFixture, path: &str) -> i64 {
    fs::metadata(fixture.disk_path(path)).expect("the file").modified().expect("its time").duration_since(UNIX_EPOCH).expect("after 1970").as_secs() as i64
}

fn presence_of(fixture: &ItemFixture, row: &Item) -> ContentPresence {
    fixture.find_item(row.local_item_id).expect("the row").content_presence
}

fn begin_download_step(fixture: &ItemFixture, row: &Item) {
    assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Download, item: row.local_item_id, other_items: Vec::new(), destination: None, sent: 0, total: 0 }));
}

#[test]
fn the_decision_of_a_landing_has_a_case_for_each_row_it_can_meet() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let shown = fixture.show_onedrive_file("docs/report.txt", b"the report");
    let (place, content) = (shown.local.clone().unwrap().place.unwrap(), shown.local.clone().unwrap().content.unwrap());
    let (for_a_read, for_the_plan) = (content_unchanged(&shown), LandingCondition::VersionsUnchanged { item: ItemVersions::of(&shown) });
    let link = LandingDecision::Link { place: place.clone(), onedrive_item_id: shown.onedrive_item.clone().unwrap().item_id, downloaded: content };

    assert_eq!(decide_landing(&for_a_read, Some(&shown)), link);
    assert_eq!(decide_landing(&for_the_plan, Some(&shown)), link);
    assert_eq!(decide_landing(&for_a_read, None), LandingDecision::RowsChanged, "the row went");
    assert_eq!(decide_landing(&for_a_read, Some(&directory)), LandingDecision::RowsChanged, "no file");

    let on_disk = Item { content_presence: ContentPresence::OnDisk, ..shown.clone() };
    assert_eq!(decide_landing(&for_a_read, Some(&on_disk)), LandingDecision::AlreadyOnDisk { place });
    assert_eq!(decide_landing(&for_the_plan, Some(&on_disk)), LandingDecision::RowsChanged, "the plan's step was planned from a file that is not on disk");

    let not_shown = Item { local: None, ..shown.clone() };
    assert_eq!(decide_landing(&for_a_read, Some(&not_shown)), LandingDecision::RowsChanged);
    let unbound = Item { onedrive_item: None, ..shown.clone() };
    assert_eq!(decide_landing(&for_a_read, Some(&unbound)), LandingDecision::RowsChanged);

    // Another eTag of OneDrive's: nothing to a read, another row to the plan.
    let listed_again = Item { onedrive_etag: Some("e-later".into()), ..shown.clone() };
    assert!(matches!(decide_landing(&for_a_read, Some(&listed_again)), LandingDecision::Link { .. }));
    assert_eq!(decide_landing(&for_the_plan, Some(&listed_again)), LandingDecision::RowsChanged);
}

#[test]
fn a_landed_file_is_named_marked_with_id_state_and_ctag_and_then_on_disk_in_its_row() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let shown = fixture.show_onedrive_file("docs/report.txt", b"the report");
    let landing = make_landing(&fixture);

    let landed = land(&fixture, &landing, b"the report", &content_unchanged(&shown));

    assert_eq!(landed, ItemLandingOutcome::Landed { place: shown.local.clone().unwrap().place.unwrap(), path: "docs/report.txt".into() });
    assert_eq!(fixture.read_disk_file("docs/report.txt").as_deref(), Some(&b"the report"[..]));
    assert_eq!(fixture.read_disk_marks("docs/report.txt"), DiskMarksRead { onedrive_item_id: Some("docs!report.txt".into()), is_whole: true, ctag: Some(format!("c-{}", qx(b"the report"))) });
    assert_eq!(disk_mtime(&fixture, "docs/report.txt"), ONEDRIVE_MTIME, "the time of the content");
    assert_eq!(fixture.read_disk_marks("docs").onedrive_item_id.as_deref(), Some("docs"), "the directory above was made and marked");
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert_eq!(count_records(&fixture), 0, "the commit took the record");
    fixture.assert_rows_and_disk_agree();
    fixture.run_start_check();
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 10)), Vec::new(), "the check at start reads the landed copy as clean");
}

#[test]
fn the_directory_of_a_landed_file_is_synced_before_the_row_is_written() {
    // What can be seen of the order: once the file has its name — which `link_file` gives
    // it and syncs the directory for — the row still says not on disk, and the record
    // stands; only the commit says on disk, and takes the record.
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let landing = make_landing(&fixture);
    let (store, folder, id) = (fixture.store.clone(), fixture.folder(), shown.local_item_id);

    let seen_between = Arc::new(std::sync::Mutex::new(None));
    let seen = Arc::clone(&seen_between);
    let landed = land_with(&fixture, &landing, b"the report", &content_unchanged(&shown), move || {
        let (row, records) = store.call_blocking(move |store| Ok((store.item_store().reader().find_item(id)?, store.item_store().list_place_change_records()?))).expect("the rows");
        *seen.lock().unwrap() = Some((folder.join("report.txt").is_file(), row.expect("the row").content_presence, records));
    });

    assert!(matches!(landed, ItemLandingOutcome::Landed { .. }));
    let (is_named, presence, records) = seen_between.lock().unwrap().take().expect("the landing was linked");
    assert!(is_named, "the file has its name before its row is written");
    assert_eq!(presence, ContentPresence::NotOnDisk);
    let content = shown.local.clone().unwrap().content.unwrap();
    assert_eq!(records.into_iter().map(|(_, record)| record).collect::<Vec<_>>(), [PlaceChangeRecord::LinkFile { item: id, place: shown.local.clone().unwrap().place.unwrap(), content }]);
    assert_eq!(count_records(&fixture), 0);
}

#[test]
fn a_landing_whose_row_has_another_content_by_now_removes_what_it_named() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let shown = fixture.show_onedrive_file("docs/report.txt", b"the report");
    let landing = make_landing(&fixture);
    let (store, id) = (fixture.store.clone(), shown.local_item_id);

    // Between the link and the commit the content becomes another.
    let landed = land_with(&fixture, &landing, b"the report", &content_unchanged(&shown), move || {
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: id })).expect("a writer's open");
    });

    assert_eq!(landed, ItemLandingOutcome::RowsChanged);
    assert_eq!(fixture.read_disk_file("docs/report.txt"), None, "what was named is removed again");
    assert!(!fixture.disk_path("docs").exists(), "and the directory made above it");
    assert_eq!(count_records(&fixture), 0, "the record was taken by itself");
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
}

#[test]
fn a_landing_after_the_file_was_opened_for_writing_links_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let landing = make_landing(&fixture);
    let id = shown.local_item_id;
    fixture.write_rows(move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: id }));

    let landed = land(&fixture, &landing, b"the report", &content_unchanged(&shown));

    assert_eq!(landed, ItemLandingOutcome::RowsChanged);
    assert_eq!(fixture.read_disk_file("report.txt"), None);
    assert_eq!(count_records(&fixture), 0);
}

#[test]
fn a_landing_while_a_listing_changed_the_content_does_not_say_the_old_content_is_the_new() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the old content");
    let landing = make_landing(&fixture);
    let old = content_unchanged(&shown);

    // A listing says OneDrive's content is another: the folder still shows the old one,
    // and the download of the old one lands as what the folder shows.
    fixture.list_onedrive_file("report.txt", b"the new content!");
    assert_eq!(land(&fixture, &landing, b"the old content", &old), ItemLandingOutcome::Landed { place: shown.local.clone().unwrap().place.unwrap(), path: "report.txt".into() });
    let row = fixture.find_item(shown.local_item_id).unwrap();
    assert_eq!(row.local.as_ref().unwrap().content, shown.local.as_ref().unwrap().content, "the row's local content is the one that was downloaded");
    assert_eq!(fixture.read_disk_marks("report.txt").ctag, Some(format!("c-{}", qx(b"the old content"))), "marked as the old content");
    fixture.assert_rows_and_disk_agree();

    // Once the folder shows the new content, a download of the old one lands nothing.
    let other = fixture.show_onedrive_file("other.txt", b"the old content");
    let old = content_unchanged(&other);
    let listed_other = fixture.list_onedrive_file("other.txt", b"the new content!");
    let versions = ItemVersions::of(&listed_other);
    assert_eq!(fixture.write_rows(move |store| store.item_store().commit_take_onedrive_content(&versions, None)), StepCommitOutcome::Written);
    assert_eq!(land(&fixture, &landing, b"the old content", &old), ItemLandingOutcome::RowsChanged);
    assert_eq!(fixture.read_disk_file("other.txt"), None, "the old content is not written as the new");
    assert_eq!(presence_of(&fixture, &other), ContentPresence::NotOnDisk);
}

#[test]
fn a_landing_at_the_place_the_row_has_now_follows_a_rename_made_meanwhile() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let landing = make_landing(&fixture);
    let moved = fixture.move_here(shown.local_item_id, "docs/moved.txt");

    let landed = land(&fixture, &landing, b"the report", &content_unchanged(&shown));

    assert_eq!(landed, ItemLandingOutcome::Landed { place: moved.local.unwrap().place.unwrap(), path: "docs/moved.txt".into() });
    assert_eq!(fixture.read_disk_file("docs/moved.txt").as_deref(), Some(&b"the report"[..]));
    assert_eq!(fixture.read_disk_file("report.txt"), None);
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_landing_of_an_item_the_folder_does_not_show_any_more_is_dropped() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let landing = make_landing(&fixture);
    fixture.remove_here(shown.local_item_id);

    assert_eq!(land(&fixture, &landing, b"the report", &content_unchanged(&shown)), ItemLandingOutcome::RowsChanged);

    assert_eq!(fixture.read_disk_file("report.txt"), None);
    assert_eq!(count_records(&fixture), 0);
}

#[test]
fn a_landing_at_a_place_the_disk_holds_though_the_row_says_not_on_disk_replaces_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    let landing = make_landing(&fixture);
    fixture.write_disk_file("report.txt", b"what no row says");

    let landed = land(&fixture, &landing, b"the report", &content_unchanged(&shown));

    assert_eq!(landed, ItemLandingOutcome::PlaceHeld { path: "report.txt".into() });
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"what no row says"[..]));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
    assert_eq!(count_records(&fixture), 0, "nothing was linked: the record went by itself");
}

#[test]
fn a_landing_of_content_the_row_says_is_on_disk_already_links_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("report.txt", b"the report");
    fixture.land_copy(shown.local_item_id, b"the report");
    let landing = make_landing(&fixture);

    let landed = land(&fixture, &landing, b"THE REPORT", &content_unchanged(&shown));

    assert!(matches!(landed, ItemLandingOutcome::Landed { .. }));
    assert_eq!(fixture.read_disk_file("report.txt").as_deref(), Some(&b"the report"[..]), "the copy that was there stays");
    assert_eq!(count_records(&fixture), 0);
}

#[test]
fn a_landing_for_the_plan_asks_every_version_and_one_for_a_read_the_content_alone() {
    let fixture = ItemFixture::new();
    let landing = make_landing(&fixture);

    // The plan's, planned before a listing said another version of OneDrive's: nothing.
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let planned_from = ItemVersions::of(&shown);
    begin_download_step(&fixture, &shown);
    fixture.list_onedrive_file("pinned.txt", b"another content");
    assert_eq!(land(&fixture, &landing, b"pinned", &LandingCondition::VersionsUnchanged { item: planned_from }), ItemLandingOutcome::RowsChanged);
    assert_eq!(fixture.read_disk_file("pinned.txt"), None, "the versions are asked before the disk is touched");

    // A read's, of the same row: the local content is still the one downloaded.
    assert!(matches!(land(&fixture, &landing, b"pinned", &content_unchanged(&shown)), ItemLandingOutcome::Landed { .. }));

    // The plan's, crossed by a listing's page between its link and its commit: undone.
    let crossed = fixture.show_onedrive_file("crossed.txt", b"crossed");
    begin_download_step(&fixture, &crossed);
    let (store, listing) = (fixture.store.clone(), fixture.find_item(crossed.local_item_id).unwrap());
    let condition = LandingCondition::VersionsUnchanged { item: ItemVersions::of(&crossed) };
    let landed = land_with(&fixture, &landing, b"crossed", &condition, move || {
        // What a page does to the row: OneDrive's eTag is another.
        let reference = listing.onedrive_item.clone().unwrap();
        store
            .call_blocking(move |store| {
                let began = store.item_store().begin_delta_listing()?;
                let row = konedrive_tree::Row { id: reference.item_id.as_str().into(), parent_id: Some(crate::fuse::item_fixture::ROOT_ONEDRIVE_ID.into()), name: "crossed.txt".into(), kind: konedrive_tree::Kind::File, size: 7, mtime: ONEDRIVE_MTIME, etag: Some("e-crossed-later".into()), ctag: Some("c-later".into()), quickxor: None, mime: None, placement: konedrive_tree::Placement::Placed };
                store.item_store().write_listed_changes(began.id, &[konedrive_tree::Change::Upsert(row)], began.began_at_count, &konedrive_tree::ListingLink::DeltaLink("link-crossed".into()))
            })
            .expect("the page is written");
    });
    assert_eq!(landed, ItemLandingOutcome::RowsChanged);
    assert_eq!(fixture.read_disk_file("crossed.txt"), None, "what was linked is removed again");
    assert_eq!(count_records(&fixture), 0);
    assert_eq!(presence_of(&fixture, &crossed), ContentPresence::NotOnDisk);
}

#[test]
fn a_landing_for_the_plan_says_its_commit_begins_before_anything_is_recorded_or_linked() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let landing = make_landing(&fixture);
    begin_download_step(&fixture, &shown);

    let landed = land(&fixture, &landing, b"pinned", &LandingCondition::VersionsUnchanged { item: ItemVersions::of(&shown) });

    assert!(matches!(landed, ItemLandingOutcome::Landed { .. }));
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::OnDisk);
    assert_eq!(fixture.running_steps.abort_requests(), Vec::new(), "a stop leaves a step whose commit has begun");
    assert_eq!(fixture.running_steps.count_steps_to_settle(), 1, "the plan's loop settles it");
    fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_landing_for_a_step_a_stop_aborted_records_links_and_writes_nothing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("pinned.txt", b"pinned");
    let landing = make_landing(&fixture);
    begin_download_step(&fixture, &shown);
    assert_eq!(fixture.running_steps.abort_requests(), [shown.local_item_id], "the stop aborts the download");

    let landed = land(&fixture, &landing, b"pinned", &LandingCondition::VersionsUnchanged { item: ItemVersions::of(&shown) });

    assert_eq!(landed, ItemLandingOutcome::StepStopped);
    assert_eq!(fixture.read_disk_file("pinned.txt"), None);
    assert_eq!(presence_of(&fixture, &shown), ContentPresence::NotOnDisk);
    assert_eq!(count_records(&fixture), 0);
}

/// The daemon dies after the first half of a landing: the file is named, the record stands,
/// the row is not written.
fn die_after_the_link(fixture: &ItemFixture, row: &Item, content: &[u8]) {
    let (landing, file, condition) = (make_landing(fixture), make_downloaded_file(fixture, content), content_unchanged(row));
    fixture.run_on_write_operation_thread(move || {
        let (change, linked) = landing.link_on_record(&file, &condition).expect("the file is linked");
        // The death: the change ends with nothing undone and nothing written.
        drop((change, linked));
    });
}

#[test]
fn a_death_after_the_record_or_after_the_link_is_settled_by_the_check_at_start() {
    let fixture = ItemFixture::new();
    fixture.show_onedrive_directory("docs");

    // After the record, before the link: nothing is on disk.
    let recorded = fixture.show_onedrive_file("docs/recorded.txt", b"recorded");
    let record = PlaceChangeRecord::LinkFile { item: recorded.local_item_id, place: recorded.local.clone().unwrap().place.unwrap(), content: recorded.local.clone().unwrap().content.unwrap() };
    fixture.write_rows(move |store| store.item_store().record_place_change(&record));
    // After the link: the row still has the recorded content.
    let linked = fixture.show_onedrive_file("docs/linked.txt", b"linked");
    die_after_the_link(&fixture, &linked, b"linked");
    assert_eq!(count_records(&fixture), 2);

    fixture.run_start_check();

    assert_eq!(count_records(&fixture), 0, "the check settled both records");
    assert_eq!(presence_of(&fixture, &recorded), ContentPresence::NotOnDisk);
    assert_eq!(presence_of(&fixture, &linked), ContentPresence::OnDisk, "the copy that was linked is the row's");
    assert_eq!(fixture.read_disk_file("docs/linked.txt").as_deref(), Some(&b"linked"[..]));
    fixture.assert_rows_and_disk_agree();
    assert_eq!(fixture.read_rows(|reader| reader.list_unsent(None, 10)), Vec::new(), "nothing is to send that nobody did");
}
