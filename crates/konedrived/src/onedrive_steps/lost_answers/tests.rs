use std::ffi::OsStr;

use konedrive_graph::drive::{Detail, DriveError, Status};
use konedrive_tree::{ContentVersion, ItemPlace, LocalItemId};
use serde_json::{json, Value};

use super::*;
use crate::fake_onedrive::qx;
use crate::mount::item_fixture::ItemFixture;
use crate::onedrive_steps::confirmed_item::tests::{drive_item, versions};
use crate::onedrive_steps::tests::StepsFolder;

fn drive() -> DriveId {
    DriveId::new("d1").expect("a drive id")
}

fn reference(id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: drive(), item_id: OneDriveItemId::new(id) }
}

fn found(item: Value, is_bound: bool, is_held_by_upload_session: bool) -> FoundItem {
    FoundItem { item: drive_item(item), is_bound, is_held_by_upload_session }
}

fn file(hash: Option<&str>, size: u64, etag: &str, ctag: &str, parent: &str, name: &str) -> Value {
    json!({ "id": "F", "name": name, "eTag": etag, "cTag": ctag, "size": size, "parentReference": { "id": parent }, "file": { "hashes": { "quickXorHash": hash } } })
}

fn directory(child_count: Option<u64>) -> Value {
    let mut value = json!({ "id": "D", "name": "New", "eTag": "e-d", "parentReference": { "id": "P" }, "folder": {} });
    if let Some(child_count) = child_count {
        value["folder"]["childCount"] = json!(child_count);
    }
    value
}

/// The fate, as a word: what a table compares.
fn fate(request: &SentRequest<'_>, item: &ItemVersions, found: Option<&FoundItem>) -> &'static str {
    match decide_lost_answer_fate(request, item, found, &drive()) {
        Ok(LostAnswerFate::WentThrough(Some(_))) => "went through",
        Ok(LostAnswerFate::WentThrough(None)) => "went through, gone",
        Ok(LostAnswerFate::DidNotGoThrough) => "did not go through",
        Ok(LostAnswerFate::ItemChanged) => "item changed",
        Ok(LostAnswerFate::NameHeld { .. }) => "name held",
        Err(_) => "unreadable",
    }
}

#[test]
fn a_directory_of_the_name_is_the_steps_own_only_when_it_is_empty_and_no_row_is_bound_to_it() {
    let (parent, item) = (reference("P"), versions(None, None));
    let request = SentRequest::MakeDirectory { parent: &parent, name: OsStr::new("new") };
    let cases = [
        (None, "did not go through"),
        (Some(found(directory(Some(0)), false, false)), "went through"),
        (Some(found(directory(Some(0)), true, false)), "name held"),
        (Some(found(directory(Some(2)), false, false)), "name held"),
        // A count that is not given counts as not empty.
        (Some(found(directory(None), false, false)), "name held"),
        (Some(found(file(Some("h"), 1, "e", "c", "P", "new"), false, false)), "name held"),
    ];
    for (found, expected) in cases {
        assert_eq!(fate(&request, &item, found.as_ref()), expected, "{found:?}");
    }
    let held = decide_lost_answer_fate(&request, &item, Some(&found(directory(Some(2)), false, false)), &drive());
    assert_eq!(held, Ok(LostAnswerFate::NameHeld { holder_name: "New".into() }), "spelt as OneDrive has it");
}

#[test]
fn a_file_of_the_name_is_the_steps_own_only_with_the_hash_sent_unbound_and_no_sessions_empty_file() {
    let (parent, item) = (reference("P"), versions(None, None));
    let empty_hash = qx(b"");
    let request = SentRequest::SendNewFile { parent: &parent, name: OsStr::new("a.txt"), sent_hash: "h" };
    let empty_request = SentRequest::SendNewFile { parent: &parent, name: OsStr::new("a.txt"), sent_hash: &empty_hash };
    let cases = [
        (request, None, "did not go through"),
        (request, Some(found(file(Some("h"), 5, "e", "c", "P", "a.txt"), false, false)), "went through"),
        (request, Some(found(file(Some("h"), 5, "e", "c", "P", "a.txt"), true, false)), "name held"),
        (request, Some(found(file(Some("other"), 5, "e", "c", "P", "a.txt"), false, false)), "name held"),
        (request, Some(found(directory(Some(0)), false, false)), "name held"),
        // An empty file with no hash holds the empty content.
        (empty_request, Some(found(file(None, 0, "e", "c", "P", "a.txt"), false, false)), "went through"),
        (empty_request, Some(found(file(Some(&empty_hash), 0, "e", "c", "P", "a.txt"), false, false)), "went through"),
        // The empty file of an upload session of this folder is never adopted.
        (empty_request, Some(found(file(None, 0, "e", "c", "P", "a.txt"), false, true)), "name held"),
        (request, Some(found(file(None, 0, "e", "c", "P", "a.txt"), false, false)), "name held"),
    ];
    for (request, found, expected) in cases {
        assert_eq!(fate(&request, &item, found.as_ref()), expected, "{request:?} {found:?}");
    }
}

#[test]
fn a_request_about_an_item_is_told_by_what_the_item_is_now() {
    let parent = reference("P2");
    let planned = versions(Some("e-planned"), None);
    let by_ctag = versions(None, Some(ContentVersion::Hashed { size: 1, mtime: 1, mtime_nsec: 0, hash: None, ctag: Some("c-planned".to_owned()) }));
    let no_guard = versions(None, None);
    let content = SentRequest::SendContent { sent_hash: "h" };
    let moved = SentRequest::SendMove { parent: &parent, name: OsStr::new("b.txt") };
    let at = |hash: &str, etag: &str, ctag: &str, parent: &str, name: &str| Some(found(file(Some(hash), 5, etag, ctag, parent, name), true, false));
    let cases = [
        (content, &planned, None, "item changed"),
        (content, &planned, at("h", "e-new", "c-new", "P", "a.txt"), "went through"),
        (content, &planned, at("old", "e-planned", "c", "P", "a.txt"), "did not go through"),
        (content, &planned, at("another", "e-other", "c", "P", "a.txt"), "item changed"),
        (moved, &planned, None, "item changed"),
        (moved, &planned, at("old", "e-new", "c", "P2", "b.txt"), "went through"),
        // Graph spells an item's id in more than one case.
        (moved, &planned, at("old", "e-new", "c", "p2", "b.txt"), "went through"),
        (moved, &planned, at("old", "e-planned", "c", "P", "a.txt"), "did not go through"),
        (moved, &planned, at("old", "e-other", "c", "P", "elsewhere"), "item changed"),
        (moved, &planned, at("old", "e-other", "c", "P2", "B.TXT"), "item changed"),
        // Guarded by the cTag of the synced content: as planned while it has that cTag.
        (moved, &by_ctag, at("old", "e-any", "c-planned", "P", "a.txt"), "did not go through"),
        (moved, &by_ctag, at("old", "e-any", "c-other", "P", "a.txt"), "item changed"),
        (moved, &no_guard, at("old", "e", "c", "P", "a.txt"), "item changed"),
        (SentRequest::SendRemoval, &planned, None, "went through, gone"),
        (SentRequest::SendRemoval, &planned, at("old", "e-planned", "c", "P", "a.txt"), "did not go through"),
        (SentRequest::SendRemoval, &planned, at("old", "e-other", "c", "P", "a.txt"), "item changed"),
    ];
    for (request, item, found, expected) in cases {
        assert_eq!(fate(&request, item, found.as_ref()), expected, "{request:?} {found:?}");
    }
}

#[test]
fn an_item_that_went_through_and_cannot_be_read_as_one_is_the_failure() {
    let item = versions(Some("e"), None);
    let no_etag = found(json!({ "id": "F", "name": "a.txt", "parentReference": { "id": "P" }, "file": { "hashes": { "quickXorHash": "h" } } }), true, false);
    assert_eq!(fate(&SentRequest::SendContent { sent_hash: "h" }, &item, Some(&no_etag)), "unreadable");
}

/// A read that failed, by its status: certain only for a 4xx that is no 401, 408 or 429.
#[test]
fn a_failed_read_is_certain_by_its_status() {
    let failed = |status: Option<u16>| DriveError::Failed(Detail { message: "x".to_owned(), status: status.map(Status::new), code: None });
    for status in [Some(403), Some(400), Some(405), Some(410)] {
        assert!(matches!(decide_failed_read(failed(status)), FailedRead::RefusedForCertain(_)), "{status:?}");
    }
    for status in [None, Some(401), Some(408), Some(429), Some(500), Some(503)] {
        assert!(matches!(decide_failed_read(failed(status)), FailedRead::NothingCertain(_)), "{status:?}");
    }
    assert!(matches!(decide_failed_read(DriveError::SignedOut), FailedRead::NothingCertain(StepOutcome::Refused(StepRefusal::SignedOut))));
    assert_eq!(FailedRead::RefusedForCertain(StepOutcome::Refused(StepRefusal::Forbidden)).into_outcome(), StepOutcome::Refused(StepRefusal::Forbidden));
}

#[test]
fn the_request_of_a_record_is_the_request_that_was_sent() {
    let (parent, name) = (reference("P"), std::ffi::OsString::from("n"));
    let cases = [
        (RecordedRequest::MakeDirectory { parent: parent.clone(), name: name.clone() }, SentRequest::MakeDirectory { parent: &parent, name: &name }),
        (RecordedRequest::SendNewFile { parent: parent.clone(), name: name.clone(), sent_hash: "h".into() }, SentRequest::SendNewFile { parent: &parent, name: &name, sent_hash: "h" }),
        (RecordedRequest::SendContent { sent_hash: "h".into() }, SentRequest::SendContent { sent_hash: "h" }),
        (RecordedRequest::SendMove { parent: parent.clone(), name: name.clone() }, SentRequest::SendMove { parent: &parent, name: &name }),
        (RecordedRequest::SendFileRemoval, SentRequest::SendRemoval),
        (RecordedRequest::SendDirectoryRemoval, SentRequest::SendRemoval),
    ];
    for (recorded, expected) in &cases {
        assert_eq!(SentRequest::of_record(recorded), *expected);
    }
}

/// A folder with a file OneDrive has, moved here, and the record of its move's request.
async fn folder_with_a_standing_move() -> (StepsFolder, RequestRecord, LocalItemId) {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"old");
    let item = fixture.move_here(shown.local_item_id, "b.txt").local_item_id;
    let folder = StepsFolder::start(fixture).await;
    let record = RequestRecord { item, onedrive_item: Some(ItemFixture::onedrive_reference_of("a.txt")), request: RecordedRequest::SendMove { parent: StepsFolder::root(), name: "b.txt".into() }, sent_at: 5 };
    let written = record.clone();
    folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&written));
    (folder, record, item)
}

#[tokio::test]
async fn a_record_whose_item_cannot_be_read_now_stands() {
    let (folder, record, _) = folder_with_a_standing_move().await;
    for (status, times) in [(500, 1), (503, 2), (401, 2), (408, 1)] {
        folder.refuse("GET", "items/a.txt", status, times);
        let settled = folder.lost_answers.settle_lost_answer(&record).await;
        assert!(settled.is_err(), "a read answered {status}: {settled:?}");
        assert_eq!(folder.records(), [record.clone()], "a read answered {status}");
    }
}

#[tokio::test]
async fn a_record_whose_read_is_refused_for_certain_goes_and_is_left_to_a_listing() {
    for status in [403, 400] {
        let (folder, record, item) = folder_with_a_standing_move().await;
        let row_before = folder.fixture.find_item(item);
        folder.refuse("GET", "items/a.txt", status, 1);
        assert_eq!(folder.lost_answers.settle_lost_answer(&record).await, Ok(SettledRecord::LeftToListing), "{status}");
        assert_eq!((folder.records(), folder.fixture.find_item(item)), (vec![], row_before), "{status}");
    }
}

#[tokio::test]
async fn a_record_whose_row_went_or_lost_its_binding_goes() {
    // The row that had the binding is gone: nothing is read.
    let (folder, record, _) = folder_with_a_standing_move().await;
    let no_longer_bound = RequestRecord { onedrive_item: Some(ItemFixture::onedrive_reference_of("nowhere")), ..record.clone() };
    let written = no_longer_bound.clone();
    folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&written));
    assert_eq!(folder.lost_answers.settle_lost_answer(&no_longer_bound).await, Ok(SettledRecord::Settled));
    assert_eq!((folder.records(), folder.count("GET", "items")), (vec![], 0));
}

/// A step of the item ran while the record was being settled, and wrote its own: the
/// settling forgets the record it read and no later one, and commits nothing.
#[tokio::test]
async fn a_record_a_step_wrote_since_the_settling_began_is_not_forgotten() {
    let (folder, read, item) = folder_with_a_standing_move().await;
    // The request the read record was of went through.
    folder.graph.with(|cloud| cloud.rename("a.txt", "R", "b.txt"));
    let later = RequestRecord { sent_at: read.sent_at + 100, ..read.clone() };
    let written = later.clone();
    folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&written));
    let row_before = folder.fixture.find_item(item);

    assert_eq!(folder.lost_answers.settle_lost_answer(&read).await, Ok(SettledRecord::Settled));
    assert_eq!((folder.records(), folder.fixture.find_item(item)), (vec![later], row_before));
}

/// After the lost answer of an empty new file, an empty file found where the step memory
/// keeps an opening of this folder is that session's, not the step's.
#[tokio::test]
async fn an_empty_file_found_where_a_session_or_an_opening_is_kept_is_not_the_steps_own() {
    let fixture = ItemFixture::new();
    let (empty, other) = (fixture.make_own_file("a.txt", b""), fixture.make_own_file("other.txt", b"x"));
    let versions = fixture.versions_of(empty.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));
    let (root, empty_hash) = (StepsFolder::root(), qx(b""));
    let request = SentRequest::SendNewFile { parent: &root, name: OsStr::new("a.txt"), sent_hash: &empty_hash };

    // Nothing of this folder explains the empty file: it holds the content sent.
    let unexplained = folder.lost_answers.read_request_outcome(&versions, request).await;
    assert!(matches!(unexplained, Ok(LostAnswerFate::WentThrough(Some(_)))), "{unexplained:?}");

    let (other, place) = (other.local_item_id, ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a.txt".into() });
    folder.fixture.write_rows(move |store| store.step_memory().remember_upload_session_opening(other, &place, 1));
    assert_eq!(folder.lost_answers.read_request_outcome(&versions, request).await, Ok(LostAnswerFate::NameHeld { holder_name: "a.txt".into() }));
}
