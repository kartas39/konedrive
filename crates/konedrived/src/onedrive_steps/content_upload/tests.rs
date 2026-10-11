use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::FRAGMENT_UNIT;
use konedrive_graph::pool::Class;
use konedrive_tree::{ContentVersion, ItemMemory, ItemPlace, ItemVersions, LocalItemId, OneDriveStep, UploadSession, UploadSessionAddress};
use serde_json::json;
use wiremock::ResponseTemplate;

use super::*;
use crate::fake_onedrive::qx;
use crate::mount::item_fixture::ItemFixture;
use crate::onedrive_steps::tests::StepsFolder;
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepObstacle, StepOutcome, StepRefusal};

/// `size` bytes no two fragments of which are alike.
fn bytes(size: u64) -> Vec<u8> {
    (0..size).map(|at| (at % 251) as u8).collect()
}

/// A content of two fragments, the second a short one.
fn two_fragments() -> Vec<u8> {
    bytes(FRAGMENT_UNIT + 10)
}

#[test]
fn an_upload_begins_by_the_size_and_the_session_the_step_memory_still_says() {
    let session = UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: None, next_offset: 5, content: ContentVersion::ChangedHere { size: 9, mtime: 1, mtime_nsec: 0, change_count: 1 }, empty_file_place: None };
    let with_session = ItemMemory { upload_session: Some(session.clone()), ..ItemMemory::default() };
    assert_eq!(decide_upload_start(&ItemMemory::default(), 0), UploadStart::Empty);
    assert_eq!(decide_upload_start(&with_session, 0), UploadStart::Empty, "an empty content is one request, whatever is kept");
    assert_eq!(decide_upload_start(&with_session, 9), UploadStart::Resume(session));
    assert_eq!(decide_upload_start(&ItemMemory::default(), 9), UploadStart::Open);
}

#[test]
fn an_upload_stops_between_fragments_for_a_hold_and_for_a_full_onedrive() {
    let stopped = Some(StepOutcome::NotRun(StepObstacle::SendingStopped));
    let no_space = Some(StepOutcome::Refused(StepRefusal::NoSpace { free_space: None }));
    let cases = [((false, false), None), ((true, false), stopped.clone()), ((true, true), stopped), ((false, true), no_space)];
    for ((is_content_held, is_onedrive_full), expected) in cases {
        assert_eq!(decide_stop_between_fragments(is_content_held, is_onedrive_full), expected, "held {is_content_held}, full {is_onedrive_full}");
    }
}

#[test]
fn an_upload_that_ended_with_another_content_is_sent_again() {
    let content = |size: u64| ContentVersion::Hashed { size, mtime: 1, mtime_nsec: 0, hash: Some(format!("h{size}")), ctag: None };
    assert_eq!(decide_own_upload_follow_up(&OwnUploadContent { onedrive_content: content(1), local_content: content(2) }), OwnUploadFollowUp::SendAgain);
}

/// A folder with a new file `a.txt` of `content`, and the step that sends it.
async fn folder_with_a_new_file(content: &[u8]) -> (StepsFolder, OneDriveStep) {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("a.txt", content);
    let step = OneDriveStep::SendNewFile { item: fixture.versions_of(file.local_item_id), parent: StepsFolder::root(), name: "a.txt".into() };
    (StepsFolder::start(fixture).await, step)
}

/// A folder with a file `a.txt` OneDrive has as `old`, changed here to `content`, and the
/// step that sends the content.
async fn folder_with_a_changed_file(content: &[u8]) -> (StepsFolder, OneDriveStep) {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"old");
    fixture.land_copy(shown.local_item_id, b"old");
    fixture.change_file_here(shown.local_item_id, content);
    let step = OneDriveStep::SendContent { item: fixture.versions_of(shown.local_item_id) };
    (StepsFolder::start(fixture).await, step)
}

fn place() -> ItemPlace {
    ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a.txt".into() }
}

fn opening_count(folder: &StepsFolder) -> usize {
    folder.fixture.read_rows(|items| items.list_opening_windows(&place())).len()
}

fn cancellation_count(folder: &StepsFolder) -> usize {
    folder.fixture.read_rows(|items| items.list_upload_session_cancellations(10)).len()
}

/// Runs `step` with its second fragment throttled at both of the client's sends: the
/// upload stops with its session kept, one fragment in OneDrive.
async fn run_stopped_after_the_first_fragment(folder: &StepsFolder, step: &OneDriveStep) -> StepOutcome {
    folder.graph.with(|cloud| cloud.throttle("PUT", "upload/", 1, 0, 2));
    let outcome = folder.run(step.clone()).await;
    assert!(matches!(outcome, StepOutcome::Refused(StepRefusal::Throttled { .. })), "{outcome:?}");
    assert_eq!(folder.memory(step.item().local_item_id).upload_session.map(|session| session.next_offset), Some(FRAGMENT_UNIT));
    outcome
}

fn cloud_content(folder: &StepsFolder, path: &str) -> Option<Vec<u8>> {
    folder.graph.with(|cloud| cloud.at(path).map(|item| item.content.clone()))
}

#[tokio::test]
async fn an_empty_file_is_sent_by_one_request_and_opens_no_session() {
    let (folder, step) = folder_with_a_new_file(b"").await;
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
    assert_eq!((folder.count("POST", "createUploadSession"), folder.count("PUT", "upload/"), folder.count("PUT", "content")), (0, 0, 1));
    assert_eq!((cloud_content(&folder, "a.txt"), opening_count(&folder)), (Some(Vec::new()), 0));
}

#[tokio::test]
async fn a_new_file_is_sent_in_fragments_through_one_session_and_marked_when_its_commit_is_written() {
    let content = bytes(2 * FRAGMENT_UNIT + 100);
    let (folder, step) = folder_with_a_new_file(&content).await;
    let item = step.item().local_item_id;

    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));

    assert_eq!((folder.count("POST", "createUploadSession"), folder.count("PUT", "upload/")), (1, 3));
    assert_eq!(cloud_content(&folder, "a.txt"), Some(content.clone()));
    let row = folder.fixture.find_item(item).expect("the file's row");
    assert!(row.onedrive_item.is_some() && row.unsent_since.is_none(), "{row:?}");
    assert_eq!(row.synced_content.as_ref().map(ContentVersion::size), Some(content.len() as u64));
    // Nothing is left of the upload in the step memory, and nothing to cancel.
    assert_eq!((folder.memory(item), opening_count(&folder), cancellation_count(&folder), folder.graph.with(|cloud| cloud.open_sessions())), (ItemMemory::default(), 0, 0, 0));
    let marks = folder.fixture.read_disk_marks("a.txt");
    assert_eq!((marks.onedrive_item_id, marks.is_whole), (row.onedrive_item.map(|item| item.item_id.as_str().to_owned()), true));
}

#[tokio::test]
async fn an_opening_whose_request_was_answered_for_certain_is_settled_and_one_whose_answer_was_lost_stays() {
    for (status, kept_openings) in [(423, 0), (403, 0), (500, 1)] {
        let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
        folder.refuse("POST", "createUploadSession", status, 1);
        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Refused(_)), "{status}: {outcome:?}");
        assert_eq!((opening_count(&folder), folder.memory(step.item().local_item_id).upload_session), (kept_openings, None), "answered {status}");
    }
}

#[tokio::test]
async fn an_opening_is_kept_before_the_request_and_the_session_before_its_first_byte() {
    let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
    let folder = Arc::new(folder);
    let item = step.item().local_item_id;
    // The opening's request is held in flight: the opening is on record while it is.
    folder.graph.with(|cloud| cloud.delay("POST", "createUploadSession", Duration::from_millis(300), 1));
    // Both sends of the first fragment are throttled: no byte reaches the session.
    folder.graph.with(|cloud| cloud.throttle("PUT", "upload/", 0, 0, 2));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("POST", "createUploadSession") == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!((opening_count(&folder), folder.memory(item).upload_session), (1, None));

    let outcome = running.await.expect("the step's task");
    assert!(matches!(outcome, StepOutcome::Refused(StepRefusal::Throttled { .. })), "{outcome:?}");
    let session = folder.memory(item).upload_session.expect("the session is kept before its first byte");
    assert_eq!((session.next_offset, session.empty_file_place, opening_count(&folder)), (0, Some(place()), 0), "the address tells the empty file from now on");
}

#[tokio::test]
async fn a_session_of_the_same_content_is_gone_on_with_from_where_onedrive_says_it_got() {
    let content = bytes(2 * FRAGMENT_UNIT + 100);
    let (folder, step) = folder_with_a_new_file(&content).await;
    run_stopped_after_the_first_fragment(&folder, &step).await;
    let sent_before = folder.count("PUT", "upload/");

    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
    // The hash sent is that of the whole file, though the first fragment went in an earlier run: the answer's hash was asked.
    assert_eq!(cloud_content(&folder, "a.txt"), Some(content));
    assert_eq!((folder.count("POST", "createUploadSession"), folder.count("PUT", "upload/") - sent_before, folder.count("GET", "upload/")), (1, 2, 2), "one status read by the client's retry, one by the next run");
}

#[tokio::test]
async fn a_hold_between_two_fragments_keeps_the_session() {
    let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
    // Held before anything is sent.
    folder.fixture.sending_holds.set_paused(Some(None));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::NotRun(StepObstacle::SendingStopped));
    assert_eq!(folder.count("POST", "createUploadSession"), 0);

    folder.fixture.sending_holds.set_paused(None);
    run_stopped_after_the_first_fragment(&folder, &step).await;
    let sent_before = folder.count("PUT", "upload/");
    folder.fixture.sending_holds.set_paused(Some(None));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::NotRun(StepObstacle::SendingStopped));
    assert_eq!((folder.count("PUT", "upload/"), cancellation_count(&folder)), (sent_before, 0));
    assert!(folder.memory(step.item().local_item_id).upload_session.is_some());

    folder.fixture.sending_holds.set_paused(None);
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
}

#[tokio::test]
async fn a_full_onedrive_between_two_fragments_keeps_the_session() {
    let content = two_fragments();
    let (folder, step) = folder_with_a_new_file(&content).await;
    // Less than a mebibyte is left: the content does not land, and OneDrive is full.
    folder.graph.with(|cloud| cloud.free = Some(100));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::NoSpace { free_space: None }));
    assert!(folder.fixture.onedrive_space.is_full());

    // Nothing is sent while OneDrive is full.
    let requests_before = folder.graph.with(|cloud| cloud.log.len());
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::NoSpace { free_space: None }));
    assert_eq!(folder.graph.with(|cloud| cloud.log.len()), requests_before);
}

#[tokio::test]
async fn a_file_too_big_for_the_space_left_is_its_own_refusal() {
    let (folder, step) = folder_with_a_new_file(&bytes(3 << 20)).await;
    folder.graph.with(|cloud| cloud.free = Some(2 << 20));
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::NoSpace { free_space: Some(2 << 20) }));
    assert!(!folder.fixture.onedrive_space.is_full(), "the file alone is too big");
}

#[tokio::test]
async fn a_file_changed_between_two_fragments_abandons_the_session() {
    let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
    let folder = Arc::new(folder);
    let item = step.item().local_item_id;
    // The first fragment is held in flight, and the file's time changes meanwhile.
    folder.graph.with(|cloud| cloud.delay("PUT", "upload/", Duration::from_millis(300), 1));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("PUT", "upload/") == 0 {
        tokio::task::yield_now().await;
    }
    folder.fixture.set_disk_mtime("a.txt", folder.fixture.now.load(Ordering::SeqCst) + 100);

    assert_eq!(running.await.expect("the step's task"), StepOutcome::NotRun(StepObstacle::ContentChanged));
    assert_eq!((folder.memory(item).upload_session, cancellation_count(&folder), folder.count("PUT", "upload/")), (None, 1, 1), "given up: its cancel is left to send");
}

#[tokio::test]
async fn a_fragment_refused_for_good_abandons_the_session_and_one_refused_for_now_keeps_it() {
    for (status, kept_session, queued_cancels) in [(400, false, 1), (423, true, 0), (500, true, 0)] {
        let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
        folder.refuse("PUT", "upload/", status, 1);
        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Refused(_)), "{status}: {outcome:?}");
        assert_eq!((folder.memory(step.item().local_item_id).upload_session.is_some(), cancellation_count(&folder)), (kept_session, queued_cancels), "a fragment answered {status}");
    }
}

#[tokio::test]
async fn a_session_found_gone_otherwise_is_forgotten_and_said() {
    let content = two_fragments();
    let (folder, step) = folder_with_a_new_file(&content).await;
    run_stopped_after_the_first_fragment(&folder, &step).await;
    folder.graph.with(|cloud| cloud.expire_sessions());

    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::UploadSessionGone));
    assert_eq!((folder.memory(step.item().local_item_id).upload_session, cancellation_count(&folder)), (None, 0), "ended: nothing is left to cancel");

    // Its next try opens a new session.
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
    assert_eq!((folder.count("POST", "createUploadSession"), cloud_content(&folder, "a.txt")), (2, Some(content)));
}

/// The last fragment went in and its answer was lost: the session is gone, the content is
/// there, and the next try adopts it with nothing sent.
#[tokio::test]
async fn a_session_found_gone_whose_content_is_in_onedrive_is_the_upload_that_went_through() {
    for is_new_file in [true, false] {
        let content = bytes(1000);
        let (folder, step) = if is_new_file { folder_with_a_new_file(&content).await } else { folder_with_a_changed_file(&content).await };
        folder.graph.with(|cloud| cloud.lose_answers("PUT", "upload/", 1));
        let lost = folder.run(step.clone()).await;
        assert!(matches!(lost, StepOutcome::Refused(StepRefusal::Transient { .. })), "{lost:?}");
        assert!(folder.memory(step.item().local_item_id).upload_session.is_some(), "the session is the record of the request");
        assert_eq!(folder.records(), [], "an upload through a session writes no request record");

        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Written { .. }), "new file {is_new_file}: {outcome:?}");
        assert_eq!((folder.count("PUT", "upload/"), folder.count("POST", "createUploadSession"), cloud_content(&folder, "a.txt")), (1, 1, Some(content)));
        assert!(folder.fixture.find_item(step.item().local_item_id).is_some_and(|row| row.unsent_since.is_none() && row.onedrive_item.is_some()));
    }
}

/// An answer of the last fragment as OneDrive would give it for an item `id` that holds
/// another content than was sent.
fn answer_with_another_hash(id: &str, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "id": id, "name": "a.txt", "eTag": "e-wrong", "cTag": "c-wrong", "size": 1000,
        "parentReference": { "id": "R", "driveId": "d1" },
        "file": { "hashes": { "quickXorHash": "another-hash" } },
        "fileSystemInfo": { "lastModifiedDateTime": "2024-05-01T10:00:00Z" },
    }))
}

#[tokio::test]
async fn an_answer_with_another_hash_for_a_new_file_is_remembered_and_its_removal_asked() {
    let content = bytes(1000);
    let (folder, step) = folder_with_a_new_file(&content).await;
    let item = step.item().local_item_id;
    folder.graph.with(|cloud| cloud.script("PUT", "upload/", answer_with_another_hash("WRONG", 201), 1));
    // Its removal fails for now: it stays remembered.
    folder.refuse("DELETE", "items/WRONG", 500, 1);

    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::ContentMismatch));
    let kept = folder.memory(item);
    assert_eq!(kept.mismatched_item.as_ref().map(|left| left.item.clone()), Some(ItemFixture::onedrive_reference_of("WRONG")));
    assert_eq!((kept.upload_session, folder.count("DELETE", "items/WRONG"), folder.fixture.find_item(item).and_then(|row| row.onedrive_item)), (None, 1, None), "nothing is committed");
    // The scripted answer was not carried out: the session is open still in the fake, and is no more this step's.
    folder.graph.with(|cloud| cloud.expire_sessions());

    // The next try removes what the upload left before it sends the file again: it is gone by now.
    assert!(matches!(folder.run(step).await, StepOutcome::Written { .. }));
    assert_eq!((folder.memory(item).mismatched_item, cloud_content(&folder, "a.txt")), (None, Some(content)));
    assert_eq!(folder.count("GET", "items/WRONG"), 1);
}

#[tokio::test]
async fn an_answer_with_another_hash_whose_item_is_removed_at_once_is_forgotten() {
    let (folder, step) = folder_with_a_new_file(&bytes(1000)).await;
    folder.graph.with(|cloud| cloud.script("PUT", "upload/", answer_with_another_hash("WRONG", 201), 1));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::ContentMismatch));
    // The fake does not have the item: found gone, forgotten.
    assert_eq!((folder.memory(step.item().local_item_id).mismatched_item, folder.count("DELETE", "items/WRONG")), (None, 1));
    assert_eq!(folder.graph.with(|cloud| cloud.guards.last().cloned()), Some(("me/drive/items/WRONG".to_owned(), "e-wrong".to_owned())), "guarded by the answer's eTag");
}

#[tokio::test]
async fn an_item_an_earlier_upload_left_is_removed_adopted_or_left_before_the_file_is_sent_again() {
    use konedrive_tree::MismatchedItem;
    let content = bytes(1000);
    // What the fake has as the leftover, and whether a new file is sent after it was dealt with.
    for (left_content, changed_since, expected_paths, sends) in [
        (b"other content".to_vec(), false, vec!["a.txt"], true),
        (b"other content".to_vec(), true, vec!["a.txt", "left.txt"], true),
        (content.clone(), false, vec!["left.txt"], false),
    ] {
        let (folder, step) = folder_with_a_new_file(&content).await;
        let item = step.item().local_item_id;
        folder.graph.with(|cloud| cloud.add_file("LEFT", "R", "left.txt", &left_content));
        let left = MismatchedItem::of_answer(ItemFixture::onedrive_reference_of("LEFT"), Some("c-LEFT"), Some("e-LEFT"));
        folder.fixture.write_rows(move |store| store.step_memory().remember_mismatched_item(item, &left).map(|_| ()));
        if changed_since {
            folder.graph.with(|cloud| cloud.edit("LEFT", b"another's work"));
        }

        let outcome = folder.run(step.clone()).await;
        assert!(matches!(outcome, StepOutcome::Written { .. }), "{outcome:?}");
        assert_eq!(folder.cloud_paths(), expected_paths, "changed since {changed_since}");
        assert_eq!((folder.memory(item).mismatched_item, folder.count("POST", "createUploadSession")), (None, usize::from(sends)));
        let bound_to = folder.fixture.find_item(item).and_then(|row| row.onedrive_item);
        assert_eq!(bound_to == Some(ItemFixture::onedrive_reference_of("LEFT")), !sends, "adopted when it holds the content after all");
    }
}

#[tokio::test]
async fn an_answer_with_another_hash_for_an_existing_item_keeps_the_fact_of_the_own_upload() {
    let (folder, step) = folder_with_a_changed_file(&bytes(1000)).await;
    let item = step.item().local_item_id;
    let row_before = folder.fixture.find_item(item);
    folder.graph.with(|cloud| cloud.script("PUT", "upload/", answer_with_another_hash("a.txt", 200), 1));

    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::ContentMismatch));
    assert_eq!((folder.fixture.find_item(item), folder.memory(item).upload_session, folder.count("DELETE", "items")), (row_before, None, 0), "nothing is committed, and nothing removed");
    assert_eq!(folder.fixture.read_rows(move |items| items.find_own_upload_content(item)), None, "answered only once the row says OneDrive has that content");

    // The listing the refusal asks for brings what the upload left: the fact tells the plan whose content it is.
    use konedrive_tree::{Change, Kind, Placement, Row};
    let listed = Row { id: "a.txt".into(), parent_id: Some("R".into()), name: "a.txt".into(), kind: Kind::File, size: 1000, mtime: 1_714_557_600, etag: Some("e-wrong".into()), ctag: Some("c-wrong".into()), quickxor: Some("another-hash".into()), mime: None, placement: Placement::Placed };
    folder.fixture.write_listing(vec![Change::Upsert(listed)]);
    let fact = folder.fixture.read_rows(move |items| items.find_own_upload_content(item)).expect("the fact is kept for the plan");
    assert_eq!(Some(fact.local_content), step.item().local.as_ref().and_then(|local| local.content.clone()));
    assert!(matches!(fact.onedrive_content, ContentVersion::Hashed { hash: Some(hash), .. } if hash == "another-hash"));
}

#[tokio::test]
async fn a_changed_item_that_holds_the_hash_sent_is_the_upload_that_went_through() {
    let content = two_fragments();
    let (folder, step) = folder_with_a_changed_file(&content).await;
    // This step's own earlier upload: OneDrive has the content, under another eTag than the step was planned from.
    folder.graph.with(|cloud| cloud.edit("a.txt", &content));
    assert!(matches!(folder.run(step.clone()).await, StepOutcome::Written { .. }));
    assert_eq!((folder.count("PUT", "upload/"), folder.count("GET", "items/a.txt")), (0, 1));
    assert!(folder.fixture.find_item(step.item().local_item_id).is_some_and(|row| row.unsent_since.is_none()));

    // Changed by another: a changed item, and nothing is sent.
    let (folder, step) = folder_with_a_changed_file(&content).await;
    folder.graph.with(|cloud| cloud.edit("a.txt", b"another's work"));
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::ItemChanged));
    assert_eq!((folder.count("PUT", "upload/"), cloud_content(&folder, "a.txt")), (0, Some(b"another's work".to_vec())));
}

#[tokio::test]
async fn the_guard_is_asked_again_before_the_last_fragment_of_an_existing_item() {
    let (folder, step) = folder_with_a_changed_file(&two_fragments()).await;
    run_stopped_after_the_first_fragment(&folder, &step).await;
    let sent_before = folder.count("PUT", "upload/");
    // Another changed the item while the session waited.
    folder.graph.with(|cloud| cloud.edit("a.txt", b"another's work"));

    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::ItemChanged));
    assert_eq!((folder.count("PUT", "upload/"), cloud_content(&folder, "a.txt")), (sent_before, Some(b"another's work".to_vec())), "the last fragment is not sent over another's change");
    assert_eq!((folder.memory(step.item().local_item_id).upload_session, cancellation_count(&folder)), (None, 1));
}

#[tokio::test]
async fn an_existing_item_that_is_gone_or_locked_is_answered_as_its_case() {
    for (status, expected) in [(404, StepRefusal::ItemGone), (423, StepRefusal::Locked), (409, StepRefusal::ItemChanged)] {
        let (folder, step) = folder_with_a_changed_file(&two_fragments()).await;
        folder.refuse("POST", "createUploadSession", status, 1);
        assert_eq!(folder.run(step).await, StepOutcome::Refused(expected), "answered {status}");
    }
    let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
    folder.refuse("POST", "createUploadSession", 404, 1);
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::DirectoryGone));
}

/// A session of a new file is gone on with at the place of its empty file, whatever the
/// file is called here by now: the commit agrees on that place, and the rename is still to
/// send.
#[tokio::test]
async fn a_session_whose_empty_file_is_elsewhere_is_gone_on_with_and_answers_that_place() {
    let content = two_fragments();
    let (folder, step) = folder_with_a_new_file(&content).await;
    let item = step.item().local_item_id;
    run_stopped_after_the_first_fragment(&folder, &step).await;
    folder.fixture.move_here(item, "renamed.txt");
    let renamed = OneDriveStep::SendNewFile { item: folder.fixture.versions_of(item), parent: StepsFolder::root(), name: "renamed.txt".into() };

    let outcome = folder.run(renamed).await;
    assert!(matches!(outcome, StepOutcome::Written { .. }), "{outcome:?}");
    assert_eq!((folder.count("POST", "createUploadSession"), folder.cloud_paths()), (1, vec!["a.txt".to_owned()]));
    let row = folder.fixture.find_item(item).expect("the file's row");
    assert_eq!(row.synced_place, Some(place()), "the place the request named is the place agreed on");
    assert!(row.unsent_since.is_some(), "the rename is still to send");
}

#[tokio::test]
async fn a_new_file_whose_name_is_taken_is_answered_by_who_holds_it() {
    let content = two_fragments();
    // Another item.
    let (folder, step) = folder_with_a_new_file(&content).await;
    folder.graph.with(|cloud| cloud.add_file("OTHER", "R", "A.TXT", b"another"));
    assert_eq!(folder.run(step.clone()).await, StepOutcome::Refused(StepRefusal::NameTaken { place: place(), holder_name: "A.TXT".into() }));
    assert_eq!((opening_count(&folder), folder.memory(step.item().local_item_id).upload_session), (0, None));

    // The same content, no row bound to it: this step's own earlier upload.
    let (folder, step) = folder_with_a_new_file(&content).await;
    folder.graph.with(|cloud| cloud.add_file("EARLIER", "R", "a.txt", &content));
    assert!(matches!(folder.run(step.clone()).await, StepOutcome::Written { .. }));
    assert_eq!((folder.count("PUT", "upload/"), folder.fixture.find_item(step.item().local_item_id).and_then(|row| row.onedrive_item)), (0, Some(ItemFixture::onedrive_reference_of("EARLIER"))));

    // The empty file of a session nobody here knows: waited for, never removed.
    let (folder, step) = folder_with_a_new_file(&content).await;
    folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));
    assert_eq!(folder.run(step).await, StepOutcome::Refused(StepRefusal::NameHeldByUploadSession));
    assert_eq!(folder.graph.with(|cloud| cloud.placeholders()), ["a.txt"]);
}

#[tokio::test]
async fn the_upload_slot_is_held_from_the_first_request_to_the_end() {
    let (folder, step) = folder_with_a_new_file(&two_fragments()).await;
    let folder = Arc::new(folder);
    folder.graph.with(|cloud| cloud.delay("PUT", "upload/", Duration::from_millis(300), 1));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("PUT", "upload/") == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!((folder.pool.held(Class::Upload), folder.pool.held(Class::Metadata)), (1, 0));
    assert!(matches!(running.await.expect("the step's task"), StepOutcome::Written { .. }));
    assert_eq!(folder.pool.held(Class::Upload), 0);
}

#[tokio::test]
async fn a_new_file_made_and_removed_here_before_its_commit_is_removed_in_onedrive() {
    let (folder, step) = folder_with_a_new_file(&bytes(1000)).await;
    let folder = Arc::new(folder);
    let item = step.item().local_item_id;
    folder.graph.with(|cloud| cloud.delay("PUT", "upload/", Duration::from_millis(300), 1));
    let running = tokio::spawn({
        let (folder, step) = (Arc::clone(&folder), step.clone());
        async move { folder.run(step).await }
    });
    while folder.count("PUT", "upload/") == 0 {
        tokio::task::yield_now().await;
    }
    // The row goes while the last fragment is in flight; the file stays open in the step.
    folder.fixture.write_rows(move |store| store.item_store().apply(&konedrive_tree::LocalVersionChange::Removed { item }));

    let outcome = running.await.expect("the step's task");
    assert!(matches!(outcome, StepOutcome::RowsChanged(RowsChangedFollowUp::MadeItemRemoved(_))), "{outcome:?}");
    assert_eq!(folder.cloud_paths(), Vec::<String>::new());
}

/// What the versions say is sent, never the row as it is when the request leaves.
#[tokio::test]
async fn a_content_the_row_no_longer_has_is_not_sent() {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("a.txt", b"first");
    let planned: ItemVersions = fixture.versions_of(file.local_item_id);
    fixture.change_file_here(file.local_item_id, b"second content");
    let folder = StepsFolder::start(fixture).await;
    let outcome = folder.run(OneDriveStep::SendNewFile { item: planned, parent: StepsFolder::root(), name: "a.txt".into() }).await;
    assert_eq!(outcome, StepOutcome::NotRun(StepObstacle::ContentChanged));
    assert_eq!(folder.graph.with(|cloud| cloud.log.len()), 0);
}

#[test]
fn the_hash_of_nothing_is_the_fakes_hash_of_an_empty_file() {
    assert_eq!(konedrive_graph::quickxor::QuickXor::new().finish_base64(), qx(b""));
}
