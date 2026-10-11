use std::sync::atomic::Ordering;
use std::time::Duration;

use konedrive_tree::{ContentVersion, ItemPlace, LocalItemId, MismatchedItem, RecordedRequest, RequestRecord, UploadSession, UploadSessionAddress, UPLOAD_SESSION_OPENING_KEPT};

use super::*;
use crate::mount::item_fixture::ItemFixture;
use crate::onedrive_steps::tests::StepsFolder;

#[test]
fn the_next_try_is_a_minute_after_the_first_failure_and_doubles_to_an_hour() {
    let cases = [(1, None, 60), (2, None, 120), (3, None, 240), (6, None, 1920), (7, None, 3600), (40, None, 3600), (0, None, 60)];
    for (failed_count, retry_after, wait) in cases {
        assert_eq!(decide_next_cancellation_try(failed_count, retry_after, 1000), 1000 + wait, "after {failed_count} failures");
    }
    // After a throttle, not before the time OneDrive named.
    assert_eq!(decide_next_cancellation_try(1, Some(Duration::from_secs(300)), 1000), 1300);
    assert_eq!(decide_next_cancellation_try(3, Some(Duration::from_secs(10)), 1000), 1240);
}

/// A folder whose step memory has `count` abandoned sessions of new files `f0`, `f1`, …,
/// each open in the fake OneDrive; their addresses in the order they were abandoned.
async fn folder_with_abandoned_sessions(count: usize) -> (StepsFolder, Vec<UploadSessionAddress>) {
    let fixture = ItemFixture::new();
    let files: Vec<_> = (0..count).map(|number| fixture.make_own_file(&format!("f{number}"), b"content")).collect();
    let folder = StepsFolder::start(fixture).await;
    let mut addresses = Vec::new();
    for (number, file) in files.iter().enumerate() {
        let name = format!("f{number}");
        let session_id = folder.graph.with(|cloud| cloud.open_elsewhere("R", &name, 0));
        let address = UploadSessionAddress::new(format!("{}/upload/{session_id}", folder.graph.server.uri()));
        let content = file.local.as_ref().and_then(|local| local.content.clone()).expect("a file's content");
        let session = UploadSession { address: address.clone(), expires_at: None, next_offset: 0, content, empty_file_place: Some(ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.into() }) };
        let (item, unsent_since) = (file.local_item_id, file.unsent_since);
        folder.fixture.write_rows(move |store| {
            assert_eq!(store.step_memory().remember_upload_session(item, unsent_since, &session)?, konedrive_tree::UploadSessionStanding::Continued);
            store.step_memory().abandon_upload_session(item)
        });
        addresses.push(address);
    }
    (folder, addresses)
}

fn listed_cancellations(folder: &StepsFolder) -> Vec<UploadSessionAddress> {
    folder.fixture.read_rows(|items| items.list_upload_session_cancellations(100))
}

#[tokio::test]
async fn a_session_to_cancel_is_cancelled_and_forgotten() {
    let (folder, addresses) = folder_with_abandoned_sessions(2).await;
    assert_eq!(listed_cancellations(&folder), addresses);

    let round = folder.cancellations.cancel_sessions(1).await;
    assert_eq!((round.done_count, round.failed_count, round.waiting_count, round.next_try_at), (1, 0, 0, None));
    assert_eq!((listed_cancellations(&folder), folder.graph.with(|cloud| cloud.open_sessions())), (addresses[1..].to_vec(), 1), "the one abandoned first first");

    assert!(folder.cancellations.cancel_session(&addresses[1]).await);
    assert_eq!((listed_cancellations(&folder), folder.graph.with(|cloud| cloud.placeholders())), (vec![], Vec::<String>::new()));
}

#[tokio::test]
async fn a_session_onedrive_no_longer_knows_is_forgotten() {
    let (folder, _) = folder_with_abandoned_sessions(1).await;
    folder.graph.with(|cloud| cloud.expire_sessions());
    assert_eq!(folder.cancellations.cancel_sessions(5).await.done_count, 1);
    assert_eq!(listed_cancellations(&folder), []);
}

/// A cancel that fails leaves the session listed and waits; while it waits it is passed
/// over, and the one behind it is asked; at its time it is asked again.
#[tokio::test]
async fn a_cancel_that_fails_waits_for_its_next_try_and_is_passed_over_meanwhile() {
    let (folder, addresses) = folder_with_abandoned_sessions(2).await;
    let start = folder.fixture.now.load(Ordering::SeqCst);
    folder.refuse("DELETE", "upload/", 500, 1);

    let failed = folder.cancellations.cancel_sessions(1).await;
    assert_eq!((failed.done_count, failed.failed_count, failed.waiting_count, failed.next_try_at), (0, 1, 0, Some(start + 60)));
    assert_eq!(listed_cancellations(&folder), addresses);

    // The first waits: the second is asked in its place.
    let asked_before = folder.count("DELETE", "upload/");
    let behind = folder.cancellations.cancel_sessions(1).await;
    assert_eq!((behind.done_count, behind.failed_count, behind.waiting_count, behind.next_try_at), (1, 0, 1, Some(start + 60)));
    assert_eq!((listed_cancellations(&folder), folder.count("DELETE", "upload/")), (addresses[..1].to_vec(), asked_before + 1));

    // Nothing is asked of one that waits.
    let waiting = folder.cancellations.cancel_sessions(1).await;
    assert_eq!((waiting.done_count, waiting.failed_count, waiting.waiting_count), (0, 0, 1));
    assert_eq!(folder.count("DELETE", "upload/"), asked_before + 1);

    // A second failure at its time doubles the wait; then it goes through.
    folder.fixture.now.store(start + 60, Ordering::SeqCst);
    folder.refuse("DELETE", "upload/", 500, 1);
    assert_eq!(folder.cancellations.cancel_sessions(1).await.next_try_at, Some(start + 60 + 120));
    folder.fixture.now.store(start + 180, Ordering::SeqCst);
    let done = folder.cancellations.cancel_sessions(1).await;
    assert_eq!((done.done_count, done.next_try_at, listed_cancellations(&folder)), (1, None, vec![]));
}

/// A throttled cancel waits at least the time OneDrive named.
#[tokio::test]
async fn a_throttled_cancel_waits_the_time_onedrive_named() {
    let (folder, _) = folder_with_abandoned_sessions(1).await;
    let start = folder.fixture.now.load(Ordering::SeqCst);
    folder.graph.with(|cloud| cloud.throttle_429("DELETE", "upload/", 0, 600, 1));
    assert_eq!(folder.cancellations.cancel_sessions(1).await.next_try_at, Some(start + 600));
}

/// A folder whose step memory has an item an upload left, `BAD` in the fake OneDrive,
/// left to the daemon to remove: its file's row went.
async fn folder_with_a_leftover() -> StepsFolder {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("report.txt", b"the report");
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| cloud.add_file("BAD", "R", "report.txt", b"other content"));
    let (item, left) = (file.local_item_id, MismatchedItem::of_answer(ItemFixture::onedrive_reference_of("BAD"), Some("c-BAD"), Some("e-BAD")));
    folder.fixture.write_rows(move |store| store.step_memory().remember_mismatched_item(item, &left).map(|_| ()));
    folder.fixture.remove_here(item);
    folder
}

fn listed_removals(folder: &StepsFolder) -> usize {
    folder.fixture.read_rows(|items| items.list_mismatched_item_removals(100)).len()
}

#[tokio::test]
async fn an_item_an_upload_left_is_removed_while_it_is_unchanged_since_that_upload_and_forgotten() {
    let folder = folder_with_a_leftover().await;
    assert_eq!(listed_removals(&folder), 1);
    let round = folder.cancellations.remove_mismatched_items(5).await;
    assert_eq!((round.done_count, round.failed_count), (1, 0));
    assert_eq!((folder.cloud_paths(), listed_removals(&folder)), (Vec::<String>::new(), 0));
    // Guarded by the eTag just read.
    assert_eq!(folder.graph.with(|cloud| cloud.guards.clone()), [("me/drive/items/BAD".to_owned(), "e-BAD".to_owned())]);
}

#[tokio::test]
async fn an_item_an_upload_left_that_another_changed_since_is_left_and_forgotten() {
    let folder = folder_with_a_leftover().await;
    folder.graph.with(|cloud| cloud.edit("BAD", b"another's work"));
    assert_eq!(folder.cancellations.remove_mismatched_items(5).await.done_count, 1);
    assert_eq!((folder.cloud_paths(), listed_removals(&folder), folder.count("DELETE", "items")), (vec!["report.txt".to_owned()], 0, 0));
}

#[tokio::test]
async fn an_item_an_upload_left_that_is_gone_is_forgotten_and_one_that_cannot_be_read_waits() {
    let folder = folder_with_a_leftover().await;
    folder.refuse("GET", "items/BAD", 500, 1);
    let failed = folder.cancellations.remove_mismatched_items(5).await;
    assert_eq!((failed.done_count, failed.failed_count, listed_removals(&folder)), (0, 1, 1));
    assert_eq!(folder.cancellations.remove_mismatched_items(5).await.waiting_count, 1);

    folder.fixture.now.fetch_add(60, Ordering::SeqCst);
    folder.graph.with(|cloud| cloud.trash("BAD"));
    assert_eq!(folder.cancellations.remove_mismatched_items(5).await.done_count, 1);
    assert_eq!(listed_removals(&folder), 0);
}

/// The removal of a leftover a listing brought takes the OneDrive version of its row.
#[tokio::test]
async fn a_leftover_removed_by_the_daemon_takes_the_onedrive_version_of_its_listed_row() {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("report.txt", b"the report");
    let listed = fixture.list_onedrive_file("BAD", b"other content");
    let left = MismatchedItem::of_answer(ItemFixture::onedrive_reference_of("BAD"), match listed.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref()) {
        Some(ContentVersion::Hashed { ctag, .. }) => ctag.as_deref(),
        _ => None,
    }, None);
    let item = file.local_item_id;
    fixture.write_rows(move |store| store.step_memory().remember_mismatched_item(item, &left).map(|_| ()));
    fixture.remove_here(item);
    let folder = StepsFolder::start(fixture).await;

    assert_eq!(folder.cancellations.remove_mismatched_items(5).await.done_count, 1);
    let row = folder.fixture.find_item(listed.local_item_id);
    assert!(row.as_ref().is_none_or(|row| row.onedrive.is_none()), "the row does not say OneDrive has it: {row:?}");
    assert_eq!(folder.cloud_paths(), Vec::<String>::new());
}

#[tokio::test]
async fn a_record_whose_step_runs_is_passed_over_and_one_refused_for_certain_is_left_to_a_listing() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("a.txt", b"old");
    let item = fixture.move_here(shown.local_item_id, "b.txt").local_item_id;
    let folder = StepsFolder::start(fixture).await;
    let record = RequestRecord { item, onedrive_item: Some(ItemFixture::onedrive_reference_of("a.txt")), request: RecordedRequest::SendMove { parent: StepsFolder::root(), name: "b.txt".into() }, sent_at: 5 };
    let written = record.clone();
    folder.fixture.write_rows(move |store| store.step_memory().remember_request_record(&written));

    let running = crate::sync_parts::running_steps::RunningStep { class: crate::sync_parts::running_steps::StepClass::Metadata, item, other_items: Vec::new(), destination: None, sent: 0, total: 0 };
    assert!(folder.fixture.running_steps.begin(running));
    let passed_over = folder.cancellations.settle_request_records(5).await;
    assert_eq!((passed_over.done_count, passed_over.waiting_count, folder.count("GET", "items"), folder.records().len()), (0, 1, 0, 1));
    folder.fixture.running_steps.end(item);

    // A read that says nothing certain: the record waits for its next try.
    let start = folder.fixture.now.load(Ordering::SeqCst);
    folder.refuse("GET", "items/a.txt", 500, 1);
    let failed = folder.cancellations.settle_request_records(5).await;
    assert_eq!((failed.failed_count, failed.next_try_at, folder.records().len()), (1, Some(start + 60), 1));
    assert_eq!(folder.cancellations.settle_request_records(5).await.waiting_count, 1);

    // A read refused for certain: the record goes, and a listing is what is left.
    folder.fixture.now.store(start + 60, Ordering::SeqCst);
    folder.refuse("GET", "items/a.txt", 403, 1);
    let left = folder.cancellations.settle_request_records(5).await;
    assert_eq!((left.done_count, left.left_to_listing_count, left.next_try_at, folder.records()), (1, 1, None, vec![]));
}

#[tokio::test]
async fn openings_that_name_no_file_go_after_the_time_they_are_kept() {
    let fixture = ItemFixture::new();
    let (gone, kept) = (fixture.make_own_file("gone.txt", b"x").local_item_id, fixture.make_own_file("kept.txt", b"x").local_item_id);
    let folder = StepsFolder::start(fixture).await;
    let start = folder.fixture.now.load(Ordering::SeqCst);
    let place = |name: &str| ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.into() };
    let (gone_place, kept_place) = (place("gone.txt"), place("kept.txt"));
    folder.fixture.write_rows(move |store| {
        store.step_memory().remember_upload_session_opening(gone, &gone_place, start)?;
        store.step_memory().remember_upload_session_opening(kept, &kept_place, start).map(|_| ())
    });
    // The file that asked is removed: its opening names no file.
    folder.fixture.remove_here(gone);
    let windows = |folder: &StepsFolder, name: &str| {
        let place = place(name);
        folder.fixture.read_rows(move |items| items.list_opening_windows(&place)).len()
    };

    folder.fixture.now.store(start + UPLOAD_SESSION_OPENING_KEPT, Ordering::SeqCst);
    folder.cancellations.forget_old_openings().await;
    assert_eq!((windows(&folder, "gone.txt"), windows(&folder, "kept.txt")), (1, 1), "not older than it is kept");

    folder.fixture.now.store(start + UPLOAD_SESSION_OPENING_KEPT + 1, Ordering::SeqCst);
    folder.cancellations.forget_old_openings().await;
    assert_eq!((windows(&folder, "gone.txt"), windows(&folder, "kept.txt")), (0, 1), "one that names a file stays");
}
