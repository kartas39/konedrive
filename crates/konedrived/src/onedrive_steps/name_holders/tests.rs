use std::ffi::OsStr;
use std::sync::atomic::Ordering;

use konedrive_graph::drive::item::format_graph_time;
use konedrive_tree::{ItemPlace, LocalItemId, OneDriveItemReference, UploadSession, UploadSessionAddress};
use serde_json::{json, Value};

use super::*;
use crate::fake_onedrive::qx;
use crate::mount::item_fixture::ItemFixture;
use crate::onedrive_steps::confirmed_item::tests::{drive_item, versions};
use crate::onedrive_steps::tests::StepsFolder;
use crate::sync_parts::step_outcome::{StepOutcome, StepRefusal};

const NOW: i64 = 1_700_000_000;

fn address(of: &str) -> UploadSessionAddress {
    UploadSessionAddress::new(format!("https://up/{of}"))
}

/// A holder as Graph answers it: a file of `size` bytes made at `made_at`.
fn holder(size: u64, hash: Option<&str>, made_at: Option<i64>) -> Value {
    let mut value = json!({ "id": "H", "name": "Report.txt", "eTag": "e-h", "cTag": "c-h", "size": size, "parentReference": { "id": "R" }, "file": { "hashes": { "quickXorHash": hash } } });
    if let Some(made_at) = made_at {
        value["createdDateTime"] = json!(format_graph_time(made_at));
    }
    value
}

fn facts(holder: Option<Value>) -> NameHolderFacts {
    let holder = holder.map(drive_item);
    let holder_item = holder.as_ref().and_then(|holder| to_confirmed_item(holder, &konedrive_tree::DriveId::new("d1").expect("a drive id")).ok());
    NameHolderFacts { holder, holder_item, is_holder_bound: false, sent_hash: Some("sent".to_owned()), empty_file_sessions: Vec::new(), opening_windows: Vec::new(), sessions_first_seen: Vec::new() }
}

fn continued(item: i64, of: &str, expires_at: Option<i64>) -> EmptyFileSession {
    EmptyFileSession::Continued { item: LocalItemId::new(item), address: address(of), expires_at }
}

/// The item that asks is item 7 (`confirmed_item::tests::versions`).
fn decide(facts: &NameHolderFacts) -> NameHolder {
    decide_name_holder(facts, &versions(None, None), NOW)
}

#[test]
fn no_holder_by_now_is_a_name_that_is_free() {
    let mut nobody = facts(None);
    // Whatever the step memory says of the place.
    nobody.empty_file_sessions = vec![continued(9, "other", None), EmptyFileSession::Abandoned { address: address("old") }];
    assert_eq!(decide(&nobody), NameHolder::Nobody);
}

#[test]
fn the_sessions_at_the_place_decide_before_the_holder_is_looked_at() {
    let empty_holder = || facts(Some(holder(0, None, Some(NOW))));
    let with = |sessions: Vec<EmptyFileSession>, first_seen: Vec<(UploadSessionAddress, i64)>| NameHolderFacts { empty_file_sessions: sessions, sessions_first_seen: first_seen, ..empty_holder() };
    let ended = |item: i64, of: &str| NameHolder::AnotherItemsEndedSession { item: LocalItemId::new(item), address: address(of) };
    let day = 24 * 60 * 60;
    let cases = [
        // This item's own session: the upload goes on in it, whatever else holds the place.
        (with(vec![continued(9, "other", Some(NOW + 100)), continued(7, "own", None)], vec![]), NameHolder::OwnSession),
        // Another item's session that has not ended: waited for.
        (with(vec![continued(9, "other", Some(NOW))], vec![]), NameHolder::AnotherItemsSession),
        (with(vec![continued(9, "other", Some(NOW + 1))], vec![]), NameHolder::AnotherItemsSession),
        // Past the time OneDrive said it ends at.
        (with(vec![continued(9, "other", Some(NOW - 1))], vec![]), ended(9, "other")),
        // One ended and one live: the live one is waited for.
        (with(vec![continued(9, "ended", Some(NOW - 1)), continued(8, "live", Some(NOW + 1))], vec![]), NameHolder::AnotherItemsSession),
        // No end named: live for a day after it was first seen holding the name.
        (with(vec![continued(9, "other", None)], vec![]), NameHolder::AnotherItemsSession),
        (with(vec![continued(9, "other", None)], vec![(address("other"), NOW - day + 1)]), NameHolder::AnotherItemsSession),
        (with(vec![continued(9, "other", None)], vec![(address("other"), NOW - day)]), ended(9, "other")),
        (with(vec![continued(9, "other", None)], vec![(address("another"), NOW - 2 * day)]), NameHolder::AnotherItemsSession),
        // A session nothing goes on with is cancelled; a continued one goes first.
        (with(vec![EmptyFileSession::Abandoned { address: address("old") }], vec![]), NameHolder::AbandonedSession(address("old"))),
        (with(vec![EmptyFileSession::Abandoned { address: address("old") }, continued(9, "other", Some(NOW - 1))], vec![]), ended(9, "other")),
    ];
    for (facts, expected) in cases {
        assert_eq!(decide(&facts), expected, "{:?} first seen {:?}", facts.empty_file_sessions, facts.sessions_first_seen);
    }
}

#[test]
fn a_holder_with_no_session_of_this_folder_is_told_by_what_it_is() {
    let window = |first_at: i64, last_unknown_at: i64| vec![OpeningWindow { first_at, last_unknown_at }];
    let slack = 5 * 60;
    let own_empty_file = || NameHolder::OwnEmptyFile(OneDriveItemReference { drive_id: konedrive_tree::DriveId::new("d1").expect("a drive id"), item_id: konedrive_tree::OneDriveItemId::new("H") });
    let another = || NameHolder::AnotherItem { holder_name: "Report.txt".into() };
    let empty_sent = Some(qx(b""));
    let cases: Vec<(NameHolderFacts, NameHolder)> = vec![
        // An empty file no listing told of, made within a window of an opening, the clock's slack on both sides: this folder's own.
        (NameHolderFacts { opening_windows: window(NOW - 50, NOW - 10), ..facts(Some(holder(0, None, Some(NOW - 30)))) }, own_empty_file()),
        (NameHolderFacts { opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, Some(NOW - slack)))) }, own_empty_file()),
        (NameHolderFacts { opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, Some(NOW + slack)))) }, own_empty_file()),
        // Outside every window, or its time not told: of nobody known, and waited for.
        (NameHolderFacts { opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, Some(NOW + slack + 1)))) }, NameHolder::UnknownEmptyFile),
        (NameHolderFacts { opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, None))) }, NameHolder::UnknownEmptyFile),
        (facts(Some(holder(0, None, Some(NOW)))), NameHolder::UnknownEmptyFile),
        // A step that sends an empty file adopts an empty file nobody explains, and never one an opening explains.
        (NameHolderFacts { sent_hash: empty_sent.clone(), ..facts(Some(holder(0, None, Some(NOW)))) }, NameHolder::OwnEarlierUpload(facts(Some(holder(0, None, Some(NOW)))).holder_item.expect("readable"))),
        (NameHolderFacts { sent_hash: empty_sent.clone(), opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, Some(NOW)))) }, own_empty_file()),
        // The content sent, there already and no row bound to it: this step's own earlier upload.
        (facts(Some(holder(4, Some("sent"), None))), NameHolder::OwnEarlierUpload(facts(Some(holder(4, Some("sent"), None))).holder_item.expect("readable"))),
        // A listing told of the holder: another item, whatever it holds.
        (NameHolderFacts { is_holder_bound: true, ..facts(Some(holder(4, Some("sent"), None))) }, another()),
        (NameHolderFacts { is_holder_bound: true, opening_windows: window(NOW, NOW), ..facts(Some(holder(0, None, Some(NOW)))) }, another()),
        (facts(Some(holder(4, Some("other"), None))), another()),
        (NameHolderFacts { sent_hash: None, ..facts(Some(holder(4, Some("sent"), None))) }, another()),
        // A directory holds the name.
        (facts(Some(json!({ "id": "H", "name": "Report.txt", "eTag": "e", "parentReference": { "id": "R" }, "folder": { "childCount": 0 } }))), another()),
        // A holder that cannot be read as an item is nobody's to adopt or to remove.
        (NameHolderFacts { opening_windows: window(NOW, NOW), ..facts(Some(json!({ "id": "H", "name": "Report.txt", "size": 0, "file": {}, "createdDateTime": format_graph_time(NOW) }))) }, another()),
    ];
    for (facts, expected) in cases {
        assert_eq!(decide(&facts), expected, "{:?} windows {:?} bound {} sent {:?}", facts.holder, facts.opening_windows, facts.is_holder_bound, facts.sent_hash);
    }
}

/// A new file `a.txt` of the user's own, and the folder over it.
async fn folder_with_a_new_file(content: &[u8]) -> (StepsFolder, konedrive_tree::ItemVersions) {
    let fixture = ItemFixture::new();
    let file = fixture.make_own_file("a.txt", content);
    let versions = fixture.versions_of(file.local_item_id);
    (StepsFolder::start(fixture).await, versions)
}

fn place() -> ItemPlace {
    ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a.txt".into() }
}

fn opening_count(folder: &StepsFolder) -> usize {
    folder.fixture.read_rows(|items| items.list_opening_windows(&place())).len()
}

async fn find(folder: &StepsFolder, item: &konedrive_tree::ItemVersions, sent: &[u8]) -> Result<NameHolder, StepOutcome> {
    folder.name_holders.find_name_holder(item, &StepsFolder::root(), OsStr::new("a.txt"), Some(&qx(sent))).await
}

#[tokio::test]
async fn an_empty_unlisted_file_made_within_an_opening_window_is_this_folders_and_is_removed() {
    let (folder, item) = folder_with_a_new_file(b"content").await;
    // The fake makes an empty file at the wall clock's time: the opening is kept at that time.
    let now = crate::clock::unix_now();
    folder.fixture.now.store(now, Ordering::SeqCst);
    let id = item.local_item_id;
    folder.fixture.write_rows(move |store| store.step_memory().remember_upload_session_opening(id, &place(), now).map(|_| ()));
    folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));

    let held = find(&folder, &item, b"content").await;
    assert!(matches!(held, Ok(NameHolder::OwnEmptyFile(_))), "{held:?}");
    assert_eq!((folder.graph.with(|cloud| cloud.placeholders()), opening_count(&folder)), (Vec::<String>::new(), 0), "removed, and the place resolved");
}

#[tokio::test]
async fn an_own_empty_file_onedrive_does_not_remove_is_waited_for() {
    let (folder, item) = folder_with_a_new_file(b"content").await;
    let now = crate::clock::unix_now();
    folder.fixture.now.store(now, Ordering::SeqCst);
    let id = item.local_item_id;
    folder.fixture.write_rows(move |store| store.step_memory().remember_upload_session_opening(id, &place(), now).map(|_| ()));
    folder.graph.with(|cloud| {
        cloud.open_elsewhere("R", "a.txt", 0);
        cloud.refuse_placeholder_delete = true;
    });

    assert_eq!(find(&folder, &item, b"content").await, Err(StepOutcome::Refused(StepRefusal::NameHeldByUploadSession)));
    assert_eq!((folder.graph.with(|cloud| cloud.placeholders()), opening_count(&folder)), (vec!["a.txt".to_owned()], 1), "the openings stay");
}

#[tokio::test]
async fn an_empty_unlisted_file_made_outside_every_window_is_waited_for() {
    let (folder, item) = folder_with_a_new_file(b"content").await;
    let id = item.local_item_id;
    // An opening long before the empty file was made.
    folder.fixture.write_rows(move |store| store.step_memory().remember_upload_session_opening(id, &place(), 1_000).map(|_| ()));
    folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));

    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::UnknownEmptyFile));
    assert_eq!((folder.graph.with(|cloud| cloud.placeholders()), opening_count(&folder), folder.count("DELETE", "")), (vec!["a.txt".to_owned()], 0, 0), "never removed; the holder is not this folder's, and the place is resolved");
}

#[tokio::test]
async fn the_empty_file_of_a_session_nothing_goes_on_with_is_cancelled() {
    let (folder, item) = folder_with_a_new_file(b"content").await;
    let session_id = folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));
    let address = UploadSessionAddress::new(format!("{}/upload/{session_id}", folder.graph.server.uri()));
    let content = item.local.as_ref().and_then(|local| local.content.clone()).expect("a file's content");
    let session = UploadSession { address: address.clone(), expires_at: None, next_offset: 0, content, empty_file_place: Some(place()) };
    let (id, unsent_since) = (item.local_item_id, item.unsent_since);
    folder.fixture.write_rows(move |store| {
        assert_eq!(store.step_memory().remember_upload_session(id, unsent_since, &session)?, konedrive_tree::UploadSessionStanding::Continued);
        store.step_memory().abandon_upload_session(id)
    });

    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::AbandonedSession(address)));
    assert_eq!((folder.graph.with(|cloud| cloud.open_sessions()), folder.fixture.read_rows(|items| items.list_upload_session_cancellations(10)).len()), (0, 0));

    // A cancel OneDrive does not carry out: the step waits.
    let (folder, item) = folder_with_a_new_file(b"content").await;
    let session_id = folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));
    let address = UploadSessionAddress::new(format!("{}/upload/{session_id}", folder.graph.server.uri()));
    let content = item.local.as_ref().and_then(|local| local.content.clone()).expect("a file's content");
    let session = UploadSession { address, expires_at: None, next_offset: 0, content, empty_file_place: Some(place()) };
    let (id, unsent_since) = (item.local_item_id, item.unsent_since);
    folder.fixture.write_rows(move |store| {
        assert_eq!(store.step_memory().remember_upload_session(id, unsent_since, &session)?, konedrive_tree::UploadSessionStanding::Continued);
        store.step_memory().abandon_upload_session(id)
    });
    folder.refuse("DELETE", "upload/", 500, 1);
    assert_eq!(find(&folder, &item, b"content").await, Err(StepOutcome::Refused(StepRefusal::NameHeldByUploadSession)));
}

/// Another item's session past its end is forgotten, and one that names no end is seen
/// for the first time and waited for.
#[tokio::test]
async fn another_items_session_is_waited_for_until_it_ended_and_forgotten_then() {
    let fixture = ItemFixture::new();
    let (asking, other) = (fixture.make_own_file("a.txt", b"content"), fixture.make_own_file("other.txt", b"other"));
    let item = fixture.versions_of(asking.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    folder.graph.with(|cloud| cloud.open_elsewhere("R", "a.txt", 0));
    let now = folder.fixture.now.load(Ordering::SeqCst);
    let other_content = other.local.as_ref().and_then(|local| local.content.clone()).expect("a file's content");
    let (other_id, other_unsent) = (other.local_item_id, other.unsent_since);
    let keep = |expires_at: Option<i64>| {
        let session = UploadSession { address: address("others"), expires_at, next_offset: 0, content: other_content.clone(), empty_file_place: Some(place()) };
        folder.fixture.write_rows(move |store| store.step_memory().remember_upload_session(other_id, other_unsent, &session).map(|_| ()));
    };

    keep(None);
    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::AnotherItemsSession), "seen for the first time");
    folder.fixture.now.store(now + 24 * 60 * 60, Ordering::SeqCst);
    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::AnotherItemsEndedSession { item: other_id, address: address("others") }), "a day after it was first seen");
    assert_eq!(folder.memory(other_id).upload_session, None, "said to have ended");

    keep(Some(now + 24 * 60 * 60 + 10));
    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::AnotherItemsSession));
    folder.fixture.now.store(now + 24 * 60 * 60 + 11, Ordering::SeqCst);
    assert!(matches!(find(&folder, &item, b"content").await, Ok(NameHolder::AnotherItemsEndedSession { .. })));
}

#[tokio::test]
async fn a_file_with_the_hash_sent_that_no_row_is_bound_to_is_adopted_and_any_other_holder_is_another_item() {
    let (folder, item) = folder_with_a_new_file(b"content").await;
    folder.graph.with(|cloud| cloud.add_file("EARLIER", "R", "A.txt", b"content"));
    let adopted = find(&folder, &item, b"content").await;
    assert!(matches!(&adopted, Ok(NameHolder::OwnEarlierUpload(found)) if found.item == ItemFixture::onedrive_reference_of("EARLIER") && found.name == "A.txt"), "{adopted:?}");
    assert_eq!(find(&folder, &item, b"other content").await, Ok(NameHolder::AnotherItem { holder_name: "A.txt".into() }));

    // A listing told of it: another item, though it holds the content sent.
    let fixture = ItemFixture::new();
    fixture.list_onedrive_file("a.txt", b"content");
    let file = fixture.make_own_file("b.txt", b"content");
    let item = fixture.versions_of(file.local_item_id);
    let folder = StepsFolder::start(fixture).await;
    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::AnotherItem { holder_name: "a.txt".into() }));

    // Nothing holds the name any more.
    let (folder, item) = folder_with_a_new_file(b"content").await;
    assert_eq!(find(&folder, &item, b"content").await, Ok(NameHolder::Nobody));
}

#[tokio::test]
async fn the_holder_of_a_name_a_move_found_taken_is_answered_as_onedrive_spells_it() {
    let (folder, _) = folder_with_a_new_file(b"content").await;
    folder.graph.with(|cloud| cloud.add_file("HOLDER", "R", "B.TXT", b"x"));
    let root = StepsFolder::root();
    assert_eq!(folder.name_holders.find_holder_name(&root, OsStr::new("b.txt")).await, Ok(Some("B.TXT".into())));
    assert_eq!(folder.name_holders.find_holder_name(&root, OsStr::new("free.txt")).await, Ok(None));
    // A read that fails is the step's outcome: tried again.
    folder.refuse("GET", "R:/b.txt", 403, 1);
    let refused = folder.name_holders.find_holder_name(&root, OsStr::new("b.txt")).await;
    assert!(matches!(refused, Err(StepOutcome::Refused(StepRefusal::Transient { .. }))), "{refused:?}");
}
