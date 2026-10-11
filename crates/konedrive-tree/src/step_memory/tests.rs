use konedrive_reason::Reason;

use super::memory_row::read_memory;
use super::*;
use crate::item_store::tests::{is_refused, item, make_directory, make_file, place, reference, synced};
use crate::model::{ContentVersion, ItemVersion, LocalVersionChange};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// What becomes of each fact when a binding moves.
mod binding_moves;
/// Every fact against every operation that changes what it was learnt for.
mod enumeration;
/// What a listing says again of an item a fact is about.
mod listing;
/// The item an upload left with other content, told by its own id.
mod mismatched_item;
/// The requests on record until they are settled.
mod request_records;
/// What was learnt of one unsent change, and what is answered of it for the next.
mod unsent_change;

use FactStanding::{StillSaid, VersionGone};
use FailedTryStanding::{ChangeGone, Counted};
use MismatchedItemStanding::{LeftToRemove, OfFolder};
use UploadSessionStanding::{Abandoned, Continued, OfAnotherItem};

fn address(of: &str) -> UploadSessionAddress {
    UploadSessionAddress::new(format!("https://up/{of}"))
}

fn local_version(store: &mut TreeStore, id: LocalItemId) -> ItemVersion {
    item(store, id).local.expect("shown in the folder")
}

fn local_content(store: &mut TreeStore, id: LocalItemId) -> ContentVersion {
    local_version(store, id).content.expect("a file")
}

fn local_place(store: &mut TreeStore, id: LocalItemId) -> ItemPlace {
    local_version(store, id).place.expect("below the root")
}

/// The session of a new file at its local place, sending its local content.
fn new_file_session(store: &mut TreeStore, id: LocalItemId, of: &str) -> UploadSession {
    UploadSession { address: address(of), expires_at: Some(900), next_offset: 0, content: local_content(store, id), empty_file_place: Some(local_place(store, id)) }
}

fn refusal_of(store: &mut TreeStore, id: LocalItemId, reason: Reason) -> Refusal {
    Refusal { reason, version: RefusedVersion::Shown(local_version(store, id)) }
}

fn taken_at(store: &mut TreeStore, id: LocalItemId, holder_name: &str) -> TakenName {
    TakenName { place: local_place(store, id), holder_name: holder_name.into() }
}

fn temporary_toward(store: &mut TreeStore, id: LocalItemId) -> TemporaryName {
    TemporaryName { name: format!(".konedrive-swap-{id}").into(), toward: local_place(store, id) }
}

fn space_refusal(store: &mut TreeStore, id: LocalItemId, free_space: u64) -> SpaceRefusal {
    SpaceRefusal { content: local_content(store, id), free_space }
}

/// The stamp of what is unsent of `id`, as its row has it now: what a step planned from the
/// row gives with what it learns.
fn stamp(store: &mut TreeStore, id: LocalItemId) -> Option<u64> {
    store.item_store().find_item(id).unwrap().and_then(|row| row.unsent_since)
}

// Each fact of an unsent change, given as a step planned from the row as it is now gives it.

fn remember_session(store: &mut TreeStore, id: LocalItemId, session: &UploadSession) -> UploadSessionStanding {
    let unsent_since = stamp(store, id);
    store.step_memory().remember_upload_session(id, unsent_since, session).unwrap()
}

fn remember_try(store: &mut TreeStore, id: LocalItemId, reason: &Reason, next_try_at: Option<i64>) -> FailedTryStanding {
    let unsent_since = stamp(store, id);
    store.step_memory().remember_failed_try(id, unsent_since, reason, next_try_at).unwrap()
}

fn remember_space(store: &mut TreeStore, id: LocalItemId, space_refusal: &SpaceRefusal) -> FactStanding {
    let unsent_since = stamp(store, id);
    store.step_memory().remember_space_refusal(id, unsent_since, space_refusal).unwrap()
}

fn remember_refused(store: &mut TreeStore, id: LocalItemId, refusal: &Refusal) -> FactStanding {
    let unsent_since = stamp(store, id);
    store.step_memory().remember_refusal(id, unsent_since, refusal).unwrap()
}

fn remember_taken(store: &mut TreeStore, id: LocalItemId, taken_name: &TakenName) -> FactStanding {
    let unsent_since = stamp(store, id);
    store.step_memory().remember_taken_name(id, unsent_since, taken_name).unwrap()
}

fn said(store: &mut TreeStore, id: LocalItemId) -> ItemMemory {
    store.step_memory().find_item_memory(id).unwrap()
}

/// What the table keeps of `id`, whether its row says it or not.
fn kept(store: &TreeStore, id: LocalItemId) -> ItemMemory {
    read_memory(&store.conn, id).unwrap()
}

fn count(store: &TreeStore, table: &str) -> i64 {
    store.conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap()
}

/// The sessions whose cancel is left to send.
fn abandoned(store: &mut TreeStore) -> Vec<UploadSessionAddress> {
    store.step_memory().list_upload_session_cancellations(100).unwrap()
}

/// The removal the row of `id` is now.
fn unsent_removal(store: &mut TreeStore, id: LocalItemId) -> UnsentRemoval {
    UnsentRemoval::of(&item(store, id)).expect("a removal that is not sent")
}

/// The folder shows the item of OneDrive `id` again, as a step towards the folder leaves it:
/// the two sides agree on it.
fn show_again(store: &mut TreeStore, id: LocalItemId, name: &str) {
    let mut row = item(store, id);
    let version = ItemVersion { place: Some(place(ROOT, name)), content: row.synced_content.clone() };
    (row.synced_place, row.local) = (version.place.clone(), Some(version));
    crate::item_store::item_row::write_item(&store.conn, &row).unwrap();
}

fn windows(store: &mut TreeStore, at: &ItemPlace) -> Vec<(i64, i64)> {
    store.step_memory().list_opening_windows(at).unwrap().into_iter().map(|window| (window.first_at, window.last_unknown_at)).collect()
}

fn remove(store: &mut TreeStore, id: LocalItemId) {
    store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
}

fn rename(store: &mut TreeStore, id: LocalItemId, name: &str) {
    store.item_store().apply(&LocalVersionChange::Moved { item: id, place: place(ROOT, name), is_name_ignored: false }).unwrap();
}

/// A program changed the file's content: another content version from now on.
fn change_content(store: &mut TreeStore, id: LocalItemId) {
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: id }).unwrap();
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: id, size: 77, mtime: 300, mtime_nsec: 1 }).unwrap();
}

/// Every fact of a file the folder shows, written each by its operation.
fn remember_everything_of_a_shown_file(store: &mut TreeStore, file: LocalItemId) -> ItemMemory {
    let expected = ItemMemory {
        unsent_since: stamp(store, file),
        upload_session: Some(new_file_session(store, file, "1")),
        mismatched_item: Some(MismatchedItem::of_answer(reference("B"), Some("c-B"), None)),
        temporary_name: Some(temporary_toward(store, file)),
        failed_tries: Some(FailedTries { count: 1, next_try_at: Some(60), last_failure: Reason::Refused(Some("no".to_owned())) }),
        space_refusal: Some(space_refusal(store, file, 5)),
        removal_confirmation: None,
        refusal: Some(refusal_of(store, file, Reason::Forbidden)),
        taken_name: Some(taken_at(store, file, "REPORT")),
    };
    assert_eq!(remember_session(store, file, expected.upload_session.as_ref().unwrap()), Continued);
    assert_eq!(store.step_memory().remember_mismatched_item(file, expected.mismatched_item.as_ref().unwrap()).unwrap(), LeftToRemove);
    store.step_memory().remember_temporary_name(file, expected.temporary_name.as_ref().unwrap()).unwrap();
    assert_eq!(remember_try(store, file, &Reason::Refused(Some("no".to_owned())), Some(60)), Counted(1));
    assert_eq!(remember_space(store, file, expected.space_refusal.as_ref().unwrap()), StillSaid);
    assert_eq!(remember_refused(store, file, expected.refusal.as_ref().unwrap()), StillSaid);
    assert_eq!(remember_taken(store, file, expected.taken_name.as_ref().unwrap()), StillSaid);
    expected
}

/// Each fact is written by its operation and read back, all of an item as one value; an item
/// with nothing kept, and an id with no row, have an empty one.
#[test]
fn what_is_remembered_of_an_item_is_read_back_as_one_value() {
    let mut store = TreeStore::in_memory().unwrap();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let expected = remember_everything_of_a_shown_file(&mut store, file);
    assert_eq!(said(&mut store, file), expected);
    assert_eq!(said(&mut store, other), ItemMemory::default());
    assert_eq!(said(&mut store, LocalItemId::new(99)), ItemMemory::default());
}

/// A name that is not UTF-8 and a content OneDrive gave are read back as they were written.
#[test]
fn a_name_that_is_not_utf8_and_a_hashed_content_are_read_back_as_written() {
    use std::os::unix::ffi::OsStringExt;
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: file, size: 3, mtime: 300, mtime_nsec: 0 }).unwrap();
    let holder_name = std::ffi::OsString::from_vec(vec![b'R', 0xff, b'T']);
    let taken = TakenName { place: local_place(&mut store, file), holder_name };
    let session = UploadSession { address: address("1"), expires_at: None, next_offset: 5, content: local_content(&mut store, file), empty_file_place: None };
    assert_eq!(remember_taken(&mut store, file, &taken), StillSaid);
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    assert_eq!(said(&mut store, file), ItemMemory { unsent_since: stamp(&mut store, file), taken_name: Some(taken), upload_session: Some(session), ..ItemMemory::default() });
}

/// Each fact is forgotten by itself, the others staying; with the last the item's row of the
/// table goes. The session is forgotten as one that ended: no cancel is left to send.
#[test]
fn each_fact_is_forgotten_by_itself_and_the_last_takes_the_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let mut expected = remember_everything_of_a_shown_file(&mut store, file);
    store.step_memory().forget_ended_upload_session(file, &address("other")).unwrap();
    assert_eq!(said(&mut store, file), expected, "a session at another address ended: the item's stays");
    store.step_memory().forget_ended_upload_session(file, &address("1")).unwrap();
    expected.upload_session = None;
    assert_eq!(said(&mut store, file), expected, "the session ended");
    store.step_memory().forget_mismatched_item(&reference("B")).unwrap();
    expected.mismatched_item = None;
    assert_eq!(said(&mut store, file), expected, "the item an upload left is forgotten by its own id");
    let steps: [(StepFact, fn(&mut ItemMemory)); 5] = [
        (StepFact::TemporaryName, |memory| memory.temporary_name = None),
        (StepFact::FailedTries, |memory| memory.failed_tries = None),
        (StepFact::SpaceRefusal, |memory| memory.space_refusal = None),
        (StepFact::Refusal, |memory| memory.refusal = None),
        (StepFact::TakenName, |memory| {
            memory.taken_name = None;
            memory.unsent_since = None;
        }),
    ];
    for (fact, clear) in steps {
        store.step_memory().forget_fact(file, fact).unwrap();
        clear(&mut expected);
        assert_eq!(said(&mut store, file), expected, "{fact:?} forgotten");
    }
    assert_eq!((count(&store, "step_memory"), abandoned(&mut store)), (0, vec![]));
    store.step_memory().forget_fact(LocalItemId::new(99), StepFact::Refusal).unwrap();
}

/// Nothing is remembered of an id with no row.
#[test]
fn nothing_is_written_for_an_id_with_no_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (refusal, gone) = (refusal_of(&mut store, file, Reason::Forbidden), LocalItemId::new(99));
    let mut memory = store.step_memory();
    assert!(is_refused(memory.remember_failed_try(gone, Some(1), &Reason::Network, None)));
    assert!(is_refused(memory.remember_refusal(gone, Some(1), &refusal)));
    assert!(is_refused(memory.remember_refusal(ROOT, Some(1), &refusal)));
    assert!(is_refused(memory.remember_upload_session_opening(gone, &place(ROOT, "report"), 10)));
    assert!(is_refused(memory.remember_upload_session_opening(file, &place(gone, "report"), 10)));
    assert_eq!((count(&store, "step_memory"), count(&store, "upload_session_openings")), (0, 0));
}

/// A crash loses nothing that costs the user: an upload session written before its first
/// byte, a temporary name written before its request and an opening written before its
/// request are found by a store opened again on the same file, with no orderly end between.
#[test]
fn a_session_a_temporary_name_and_an_opening_are_found_again_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut store = TreeStore::open(&path).unwrap();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let (session, temporary) = (new_file_session(&mut store, file, "1"), temporary_toward(&mut store, file));
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    store.step_memory().remember_temporary_name(file, &temporary).unwrap();
    store.step_memory().remember_upload_session_opening(other, &place(ROOT, "other"), 10).unwrap();
    // The daemon dies: nothing is closed.
    std::mem::forget(store);

    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!(said(&mut store, file), ItemMemory { unsent_since: stamp(&mut store, file), upload_session: Some(session), temporary_name: Some(temporary), ..ItemMemory::default() });
    assert_eq!(windows(&mut store, &place(ROOT, "OTHER")), [(10, 10)]);
}

/// A fact does not outlive its item, whoever deletes the row: what was remembered of it
/// goes with it, and the address of its upload session is kept to be cancelled.
#[test]
fn what_is_remembered_of_an_item_goes_with_its_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    remember_everything_of_a_shown_file(&mut store, file);
    // An item OneDrive never had: its row goes when the folder ceases to show it.
    remove(&mut store, file);
    assert_eq!(store.item_store().find_item(file).unwrap(), None);
    assert_eq!((count(&store, "step_memory"), kept(&store, file)), (0, ItemMemory::default()));
    assert_eq!(abandoned(&mut store), [address("1")], "the session is still to be cancelled");
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(ROOT, "report")).unwrap(), [EmptyFileSession::Abandoned { address: address("1") }]);

    // And by a statement no operation of the store makes.
    let second = make_file(&mut store, ROOT, "second");
    let _ = remember_try(&mut store, second, &Reason::Network, Some(5));
    store.conn.execute("DELETE FROM items WHERE local_item_id = ?1", [second.as_i64()]).unwrap();
    assert_eq!(count(&store, "step_memory"), 0);
}

/// An opening is of its place: it outlives the file that asked, and goes with the directory.
#[test]
fn an_opening_outlives_the_file_that_asked_and_goes_with_its_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    let file = make_file(&mut store, directory, "report");
    store.step_memory().remember_upload_session_opening(file, &place(directory, "report"), 10).unwrap();
    remove(&mut store, file);
    assert_eq!(windows(&mut store, &place(directory, "report")), [(10, 10)], "the empty file may still hold the name");
    remove(&mut store, directory);
    assert_eq!(count(&store, "upload_session_openings"), 0);
}

/// A fact whose version is gone is not answered, and the next write for the item takes it
/// away: an upload session and a file too big after the content changed, the session to the
/// cancellations.
#[test]
fn what_was_learnt_of_a_content_is_not_answered_once_the_content_changed() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (session, big) = (new_file_session(&mut store, file, "1"), space_refusal(&mut store, file, 5));
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    assert_eq!(remember_space(&mut store, file, &big), StillSaid);

    change_content(&mut store, file);
    assert_eq!(said(&mut store, file), ItemMemory::default(), "not answered");
    assert_eq!(store.step_memory().list_space_refusal_reasons(None, 10).unwrap(), [], "no reason for the user");
    assert_eq!(kept(&store, file).upload_session, Some(session.clone()), "kept until the next write");
    assert_eq!(store.step_memory().advance_upload_session(file, &session.address, 9, None).unwrap(), Abandoned, "not gone on with");
    assert_eq!((kept(&store, file).upload_session, abandoned(&mut store)), (None, vec![address("1")]), "abandoned by that write, with its place");
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(ROOT, "report")).unwrap(), [EmptyFileSession::Abandoned { address: address("1") }]);

    let _ = remember_try(&mut store, file, &Reason::Network, None);
    let after = kept(&store, file);
    assert_eq!((after.upload_session, after.space_refusal, after.failed_tries.map(|tries| tries.count)), (None, None, Some(1)));
    assert_eq!(abandoned(&mut store), [address("1")]);
}

/// A refusal and a taken name are not answered once the item's place changed, and go with
/// the next write for the item; the failed tries and the temporary name hold no version and
/// stay: under the temporary name the item is in OneDrive, wherever it is here.
#[test]
fn what_was_learnt_of_a_place_is_not_answered_once_the_place_changed() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (refusal, temporary, taken) = (refusal_of(&mut store, file, Reason::Forbidden), temporary_toward(&mut store, file), taken_at(&mut store, file, "REPORT"));
    assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
    store.step_memory().remember_temporary_name(file, &temporary).unwrap();
    assert_eq!(remember_taken(&mut store, file, &taken), StillSaid);
    let _ = remember_try(&mut store, file, &Reason::Locked, Some(40));
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), [ItemTakenName { item: file, taken_name: taken }]);

    rename(&mut store, file, "renamed");
    let tries = Some(FailedTries { count: 1, next_try_at: Some(40), last_failure: Reason::Locked });
    let with_no_version = ItemMemory { unsent_since: stamp(&mut store, file), failed_tries: tries, temporary_name: Some(temporary), ..ItemMemory::default() };
    assert_eq!(said(&mut store, file), with_no_version);
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), []);
    assert_eq!(store.step_memory().list_refusal_reasons(None, None, 10).unwrap(), []);
    assert!(kept(&store, file).refusal.is_some(), "kept until the next write");

    store.step_memory().forget_fact(file, StepFact::SpaceRefusal).unwrap();
    assert_eq!(kept(&store, file), with_no_version);
    rename(&mut store, file, "report");
    assert_eq!(said(&mut store, file), with_no_version, "at the place again: what was taken away is not answered again");
}

/// A fact given when the row does not say it any more is not written, and the writer says
/// so; an upload session given so is abandoned at once, and its opening goes.
#[test]
fn a_fact_the_row_does_not_say_any_more_is_not_written() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (session, big, refusal) = (new_file_session(&mut store, file, "1"), space_refusal(&mut store, file, 5), refusal_of(&mut store, file, Reason::Forbidden));
    let taken = taken_at(&mut store, file, "REPORT");
    store.step_memory().remember_upload_session_opening(file, &place(ROOT, "report"), 10).unwrap();
    change_content(&mut store, file);
    rename(&mut store, file, "renamed");
    assert_eq!(remember_session(&mut store, file, &session), Abandoned);
    assert_eq!(remember_space(&mut store, file, &big), VersionGone);
    assert_eq!(remember_refused(&mut store, file, &refusal), VersionGone);
    assert_eq!(remember_taken(&mut store, file, &taken), VersionGone);
    assert_eq!((count(&store, "step_memory"), count(&store, "upload_session_openings"), abandoned(&mut store)), (0, 0, vec![address("1")]));
}

/// The upload session: how far it got is written while the address is the item's; another
/// session abandons the earlier one; one given up is abandoned until it is cancelled. An
/// address the item does not have is not gone on with, and its cancel is left too.
#[test]
fn an_upload_session_advances_is_replaced_and_is_abandoned() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let first = new_file_session(&mut store, file, "1");
    assert_eq!(remember_session(&mut store, file, &first), Continued);
    assert_eq!(store.step_memory().advance_upload_session(file, &first.address, 4096, Some(950)).unwrap(), Continued);
    assert_eq!(said(&mut store, file).upload_session, Some(UploadSession { next_offset: 4096, expires_at: Some(950), ..first.clone() }));
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(ROOT, "REPORT")).unwrap(), [EmptyFileSession::Continued { item: file, address: address("1"), expires_at: Some(950) }]);

    let second = new_file_session(&mut store, file, "2");
    assert_eq!(remember_session(&mut store, file, &second), Continued);
    assert_eq!((said(&mut store, file).upload_session, abandoned(&mut store)), (Some(second), vec![address("1")]));
    assert_eq!(store.step_memory().advance_upload_session(file, &address("1"), 1, None).unwrap(), Abandoned, "the item has another session");
    assert_eq!(store.step_memory().advance_upload_session(LocalItemId::new(99), &address("3"), 1, None).unwrap(), Abandoned, "no row");
    let other = make_file(&mut store, ROOT, "other");
    assert_eq!(store.step_memory().advance_upload_session(other, &address("2"), 1, None).unwrap(), OfAnotherItem, "the session another item's memory keeps is not this item's to advance");
    store.step_memory().forget_ended_upload_session(other, &address("2")).unwrap();
    store.step_memory().forget_ended_upload_session(other, &address("1")).unwrap();
    assert_eq!((said(&mut store, file).upload_session.map(|session| session.address), abandoned(&mut store)), (Some(address("2")), vec![address("1"), address("3")]), "nor to cancel, nor to say ended, nor is a cancel that was left its to forget");

    store.step_memory().abandon_upload_session(file).unwrap();
    assert_eq!((said(&mut store, file), abandoned(&mut store)), (ItemMemory::default(), vec![address("1"), address("3"), address("2")]), "the one abandoned first first");
    assert_eq!(store.step_memory().list_upload_session_cancellations(1).unwrap(), [address("1")]);
    store.step_memory().forget_upload_session_cancellation(&address("1")).unwrap();
    assert_eq!(abandoned(&mut store), [address("3"), address("2")]);
}

/// The failed tries are counted; the items that wait until a time are read soonest first,
/// at most as many as asked; a refresh makes every one due and keeps count and reason.
#[test]
fn the_tries_are_counted_and_the_items_that_wait_are_read_soonest_first() {
    let mut store = TreeStore::in_memory().unwrap();
    let (a, b, c) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"), make_file(&mut store, ROOT, "c"));
    assert_eq!(remember_try(&mut store, a, &Reason::Network, Some(300)), Counted(1));
    assert_eq!(remember_try(&mut store, b, &Reason::Network, Some(100)), Counted(1));
    assert_eq!(remember_try(&mut store, b, &Reason::Locked, Some(200)), Counted(2));
    assert_eq!(remember_try(&mut store, c, &Reason::Network, None), Counted(1));
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), [TimedWait { item: b, until: 200 }, TimedWait { item: a, until: 300 }], "one due at once does not wait");
    assert_eq!(store.step_memory().list_timed_waits(1).unwrap(), [TimedWait { item: b, until: 200 }]);
    assert_eq!(said(&mut store, b).failed_tries, Some(FailedTries { count: 2, next_try_at: Some(200), last_failure: Reason::Locked }));

    assert_eq!(store.step_memory().clear_next_tries().unwrap(), 2);
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), []);
    assert_eq!(said(&mut store, b).failed_tries, Some(FailedTries { count: 2, next_try_at: None, last_failure: Reason::Locked }));
    store.step_memory().forget_fact(b, StepFact::FailedTries).unwrap();
    assert_eq!(remember_try(&mut store, b, &Reason::Network, Some(1)), Counted(1), "counted anew after a step went through");
}

/// A quota read forgets the refusal of every file that fits in the free space and says the
/// free space again of the rest; the files refused for lack of space are listed and
/// counted.
#[test]
fn a_quota_read_lets_go_what_fits_and_says_the_free_space_of_the_rest() {
    let mut store = TreeStore::in_memory().unwrap();
    let (small, large) = (make_file(&mut store, ROOT, "small"), make_file(&mut store, ROOT, "large"));
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: small, size: 10, mtime: 300, mtime_nsec: 0 }).unwrap();
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: large, size: 1000, mtime: 300, mtime_nsec: 0 }).unwrap();
    let (small_fact, large_fact) = (space_refusal(&mut store, small, 5), space_refusal(&mut store, large, 5));
    assert_eq!(remember_space(&mut store, small, &small_fact), StillSaid);
    assert_eq!(remember_space(&mut store, large, &large_fact), StillSaid);
    let too_big = [ItemRefusalReason { item: small, reason: Reason::TooBig(Some((10, 5))) }, ItemRefusalReason { item: large, reason: Reason::TooBig(Some((1000, 5))) }];
    assert_eq!(store.step_memory().list_space_refusal_reasons(None, 10).unwrap(), too_big);
    assert_eq!(store.step_memory().list_space_refusal_reasons(Some(small), 10).unwrap(), too_big[1..]);
    assert_eq!((store.step_memory().count_space_refusals().unwrap(), store.step_memory().count_refusal_reasons(None).unwrap()), (2, 0));

    assert_eq!(store.step_memory().forget_fitting_space_refusals(500).unwrap(), 1);
    assert_eq!((said(&mut store, small), store.step_memory().count_space_refusals().unwrap()), (ItemMemory::default(), 1));
    assert_eq!(said(&mut store, large).space_refusal, Some(SpaceRefusal { free_space: 500, ..large_fact }));
}

/// The confirmation of a mass removal is written of the removals it is given, each while
/// the row is still that removal, and of no other row; the items whose removal is confirmed
/// are read bounded, by local item id.
#[test]
fn a_confirmation_is_written_of_a_removal_that_is_not_sent() {
    let mut store = TreeStore::in_memory().unwrap();
    let (removed, shown, never_sent, second) = (make_file(&mut store, ROOT, "removed"), make_file(&mut store, ROOT, "shown"), make_file(&mut store, ROOT, "never-sent"), make_file(&mut store, ROOT, "second"));
    for (id, onedrive_id) in [(removed, "A"), (shown, "B"), (second, "C")] {
        synced(&mut store, id, onedrive_id);
    }
    remove(&mut store, removed);
    remove(&mut store, second);
    let (of_removed, of_second) = (unsent_removal(&mut store, removed), unsent_removal(&mut store, second));
    assert_eq!(UnsentRemoval::of(&item(&mut store, shown)), None);
    let given = [of_removed, UnsentRemoval { item: shown, unsent_since: 1 }, UnsentRemoval { item: never_sent, unsent_since: 3 }, UnsentRemoval { item: LocalItemId::new(99), unsent_since: 1 }, of_second];
    assert_eq!(store.step_memory().remember_removals_confirmed(&given).unwrap(), [given[1], given[2], given[3]], "those that were not written are said");
    assert_eq!(said(&mut store, removed), ItemMemory { unsent_since: Some(of_removed.unsent_since), removal_confirmation: Some(RemovalConfirmation { unsent_since: of_removed.unsent_since }), ..ItemMemory::default() });
    assert_eq!((said(&mut store, shown), said(&mut store, never_sent)), (ItemMemory::default(), ItemMemory::default()));
    assert_eq!(store.step_memory().list_items_with_confirmed_removal(None, 10).unwrap(), [removed, second]);
    assert_eq!(store.step_memory().list_items_with_confirmed_removal(None, 1).unwrap(), [removed]);
    assert_eq!(store.step_memory().list_items_with_confirmed_removal(Some(removed), 10).unwrap(), [second]);

    store.step_memory().forget_fact(removed, StepFact::RemovalConfirmation).unwrap();
    assert_eq!((said(&mut store, removed), count(&store, "step_memory")), (ItemMemory::default(), 1));
    assert_eq!(store.step_memory().list_items_with_confirmed_removal(None, 10).unwrap(), [second]);
}

/// A fact of a removal is of that removal. The user confirmed a removal, and OneDrive
/// refused one: the folder shows the item again, as a step towards the folder leaves it,
/// and the item is removed again. The second removal is neither confirmed nor refused,
/// whether or not anything was written of the item in between; and a confirmation given
/// with the stamp of the first removal is not written of the second.
#[test]
fn a_confirmation_and_a_refusal_of_a_removal_are_not_answered_for_the_next_removal() {
    for writes_in_between in [false, true] {
        let mut store = TreeStore::in_memory().unwrap();
        let file = make_file(&mut store, ROOT, "report");
        synced(&mut store, file, "A");
        remove(&mut store, file);
        let first = unsent_removal(&mut store, file);
        let refusal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal };
        assert_eq!(store.step_memory().remember_removals_confirmed(&[first]).unwrap(), []);
        assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
        assert_eq!(store.step_memory().list_refusal_reasons(None, None, 10).unwrap(), [ItemRefusalReason { item: file, reason: Reason::Forbidden }]);

        show_again(&mut store, file, "report");
        assert_eq!((said(&mut store, file), store.step_memory().list_refusal_reasons(None, None, 10).unwrap()), (ItemMemory::default(), vec![]));
        if writes_in_between {
            let _ = remember_try(&mut store, file, &Reason::Network, None);
            store.step_memory().forget_fact(file, StepFact::FailedTries).unwrap();
        }
        remove(&mut store, file);
        let second = unsent_removal(&mut store, file);
        assert_ne!(first, second, "another removal has another stamp");
        assert_eq!(said(&mut store, file), ItemMemory::default(), "written in between: {writes_in_between}");
        assert_eq!(store.step_memory().list_items_with_confirmed_removal(None, 10).unwrap(), []);
        assert_eq!(store.step_memory().remember_removals_confirmed(&[first]).unwrap(), [first], "the removal the user was asked about is not this one");
        assert_eq!(store.step_memory().remember_refusal(file, Some(first.unsent_since), &refusal).unwrap(), VersionGone, "nor is the refusal of the first");
    }
}

/// The items OneDrive refuses for good are read by local item id, a bounded number after
/// the item given, all of them or those of one reason, and counted; a refusal the row does
/// not say any more is left out. A file refused for good and for lack of space is in both
/// lists. The refusals of one reason are forgotten together.
#[test]
fn the_items_onedrive_refused_are_read_bounded_and_counted() {
    let mut store = TreeStore::in_memory().unwrap();
    let files: Vec<LocalItemId> = (0..5).map(|n| make_file(&mut store, ROOT, &format!("file-{n}"))).collect();
    for (file, reason) in [(files[0], Reason::Forbidden), (files[1], Reason::Refused(Some("bad name".to_owned()))), (files[2], Reason::Forbidden), (files[4], Reason::Forbidden)] {
        let refusal = refusal_of(&mut store, file, reason);
        assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
    }
    let big = space_refusal(&mut store, files[0], 5);
    assert_eq!(remember_space(&mut store, files[0], &big), StillSaid);
    rename(&mut store, files[2], "renamed");

    let reason = |item, reason| ItemRefusalReason { item, reason };
    let all = [reason(files[0], Reason::Forbidden), reason(files[1], Reason::Refused(Some("bad name".to_owned()))), reason(files[4], Reason::Forbidden)];
    let memory = store.step_memory();
    assert_eq!(memory.list_refusal_reasons(None, None, 10).unwrap(), all);
    assert_eq!(memory.list_refusal_reasons(None, None, 2).unwrap(), all[..2]);
    assert_eq!(memory.list_refusal_reasons(None, Some(files[0]), 1).unwrap(), all[1..2]);
    assert_eq!(memory.list_refusal_reasons(None, Some(files[1]), 1).unwrap(), all[2..], "what the row does not say is passed over");
    assert_eq!(memory.list_refusal_reasons(Some(&Reason::Forbidden), None, 10).unwrap(), [all[0].clone(), all[2].clone()]);
    assert_eq!(memory.list_refusal_reasons(Some(&Reason::Refused(None)), None, 10).unwrap(), all[1..2], "whatever its detail");
    assert_eq!((memory.count_refusal_reasons(None).unwrap(), memory.count_refusal_reasons(Some(&Reason::Forbidden)).unwrap()), (3, 2));
    assert_eq!(memory.list_space_refusal_reasons(None, 10).unwrap(), [reason(files[0], Reason::TooBig(Some((0, 5))))], "the same file, for lack of space");

    assert_eq!(store.step_memory().forget_refusals_for(&Reason::Forbidden).unwrap(), 3, "also the one its row does not say any more");
    assert_eq!(store.step_memory().list_refusal_reasons(None, None, 10).unwrap(), all[1..2]);
}

/// The names OneDrive answered are taken are read for one directory, each with its item.
#[test]
fn the_taken_names_are_read_for_one_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    let (inside, other_inside, outside) = (make_file(&mut store, directory, "straße"), make_file(&mut store, directory, "plain"), make_file(&mut store, ROOT, "top"));
    let (taken_inside, taken_outside) = (taken_at(&mut store, inside, "STRASSE"), taken_at(&mut store, outside, "TOP"));
    assert_eq!(remember_taken(&mut store, inside, &taken_inside), StillSaid);
    assert_eq!(remember_taken(&mut store, outside, &taken_outside), StillSaid);
    let _ = remember_try(&mut store, other_inside, &Reason::Network, None);
    assert_eq!(store.step_memory().list_taken_names(directory).unwrap(), [ItemTakenName { item: inside, taken_name: taken_inside }]);
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), [ItemTakenName { item: outside, taken_name: taken_outside }]);
}

/// An opening is kept by place and time: asked again at the same place, as OneDrive compares
/// names, it keeps its first time; a certain answer takes the record its try made and puts a
/// carried one back; asked at another place, the earlier record stays and names no file.
#[test]
fn an_opening_keeps_its_first_time_and_a_certain_answer_settles_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let at = place(ROOT, "report");
    let mut memory = store.step_memory();
    assert_eq!(memory.remember_upload_session_opening(file, &at, 10).unwrap(), OpeningTry::First);
    let carried = memory.remember_upload_session_opening(file, &place(ROOT, "Report"), 50).unwrap();
    assert_eq!(carried, OpeningTry::Repeated { last_unknown_at: 10 });
    assert_eq!(windows(&mut store, &at), [(10, 50)]);
    store.step_memory().settle_answered_opening(file, carried).unwrap();
    assert_eq!(windows(&mut store, &at), [(10, 10)], "the earlier try is still not answered");

    assert_eq!(store.step_memory().remember_upload_session_opening(file, &place(ROOT, "elsewhere"), 70).unwrap(), OpeningTry::First);
    assert_eq!((windows(&mut store, &at), windows(&mut store, &place(ROOT, "elsewhere"))), (vec![(10, 10)], vec![(70, 70)]));
    store.step_memory().settle_answered_opening(file, OpeningTry::First).unwrap();
    assert_eq!((windows(&mut store, &at), windows(&mut store, &place(ROOT, "elsewhere"))), (vec![(10, 10)], vec![]));

    // A second file asks at the first place: two windows, never one from 10 to 90.
    let second = make_file(&mut store, ROOT, "second");
    store.step_memory().remember_upload_session_opening(second, &at, 90).unwrap();
    assert_eq!(windows(&mut store, &at), [(10, 10), (90, 90)]);
    assert_eq!(store.step_memory().forget_upload_session_openings_before(60).unwrap(), 1, "the one that names no file");
    assert_eq!(windows(&mut store, &at), [(90, 90)]);
    assert_eq!(store.step_memory().forget_upload_session_openings_before(1000).unwrap(), 0, "one that names a file stays");
    store.step_memory().forget_upload_session_openings_at(&place(ROOT, "REPORT")).unwrap();
    assert_eq!(count(&store, "upload_session_openings"), 0);
}

/// Once the address of the session is kept, the opening is not: the address tells the empty
/// file from now on.
#[test]
fn the_address_of_a_session_replaces_its_opening() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    store.step_memory().remember_upload_session_opening(file, &place(ROOT, "report"), 10).unwrap();
    let session = new_file_session(&mut store, file, "1");
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    assert_eq!((count(&store, "upload_session_openings"), said(&mut store, file).upload_session), (0, Some(session)));
}

/// A session that ended is forgotten, never cancelled: also when the file's content is not
/// the one it sent any more, as at the commit of an upload, which writes the content OneDrive
/// confirmed before the session is forgotten.
#[test]
fn a_session_that_ended_is_not_abandoned_though_the_content_changed_since() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let session = new_file_session(&mut store, file, "1");
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    synced(&mut store, file, "A");
    store.step_memory().forget_ended_upload_session(file, &address("1")).unwrap();
    assert_eq!((kept(&store, file), abandoned(&mut store)), (ItemMemory::default(), vec![]));
}

/// A session that ended is forgotten of the item whose memory keeps it and of no other,
/// as a session is advanced: told of an address whose cancel was left, or of another
/// item's session, the call changes nothing, and the cancel stays for the loop that sends
/// the cancels.
#[test]
fn a_session_is_said_ended_only_of_the_item_that_keeps_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let (first, second) = (new_file_session(&mut store, file, "1"), new_file_session(&mut store, other, "2"));
    assert_eq!(remember_session(&mut store, file, &first), Continued);
    assert_eq!(remember_session(&mut store, other, &second), Continued);
    store.step_memory().abandon_upload_session(file).unwrap();
    store.step_memory().forget_ended_upload_session(other, &address("1")).unwrap();
    store.step_memory().forget_ended_upload_session(file, &address("2")).unwrap();
    assert_eq!((abandoned(&mut store), said(&mut store, other).upload_session), (vec![address("1")], Some(second)));
}

/// A new try of an upload takes the session of the former content away: the first call of
/// the try queues its cancel, and what holds the name in OneDrive is then answered as a
/// session nothing goes on with, not as the item's.
#[test]
fn a_new_try_of_an_upload_abandons_the_session_of_the_former_content() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let at = place(ROOT, "report");
    store.step_memory().remember_upload_session_opening(file, &at, 10).unwrap();
    let former = new_file_session(&mut store, file, "former");
    assert_eq!(remember_session(&mut store, file, &former), Continued);
    change_content(&mut store, file);
    assert_eq!(said(&mut store, file).upload_session, None, "the plan sends the file as a new one");

    let opening_try = store.step_memory().remember_upload_session_opening(file, &at, 100).unwrap();
    assert_eq!((kept(&store, file).upload_session, abandoned(&mut store)), (None, vec![address("former")]), "abandoned by the first call of the try");
    // OneDrive answers that the name is taken: by the empty file of the former session.
    store.step_memory().settle_answered_opening(file, opening_try).unwrap();
    assert_eq!(store.step_memory().list_empty_file_sessions(&at).unwrap(), [EmptyFileSession::Abandoned { address: address("former") }]);
}

/// An address always has somewhere to go: given for an item whose row went while the
/// request that opened the session was under way, it is queued for its cancel, with the
/// place its empty file holds, and the writer says so.
#[test]
fn the_address_of_a_session_whose_item_went_is_queued_for_its_cancel() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let at = place(ROOT, "report");
    store.step_memory().remember_upload_session_opening(file, &at, 10).unwrap();
    let session = new_file_session(&mut store, file, "late");
    remove(&mut store, file);
    assert_eq!(remember_session(&mut store, file, &session), Abandoned);
    assert_eq!(abandoned(&mut store), [address("late")]);
    assert_eq!(store.step_memory().list_empty_file_sessions(&at).unwrap(), [EmptyFileSession::Abandoned { address: address("late") }]);
    assert_eq!(windows(&mut store, &at), [(10, 10)], "the opening is of the place and stays");
}

/// The first call of a new try at a place abandons also the session of another item whose
/// empty file holds that place and whose row does not say it any more: a file that was
/// renamed after its content changed, and a new file made under its former name.
#[test]
fn an_opening_abandons_the_session_another_items_row_does_not_say_at_the_place() {
    let mut store = TreeStore::in_memory().unwrap();
    let (former, live) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let (of_former, of_live) = (new_file_session(&mut store, former, "former"), new_file_session(&mut store, live, "live"));
    assert_eq!(remember_session(&mut store, former, &of_former), Continued);
    assert_eq!(remember_session(&mut store, live, &of_live), Continued);
    change_content(&mut store, former);
    rename(&mut store, former, "renamed");
    rename(&mut store, live, "elsewhere");
    let at = place(ROOT, "report");
    assert_eq!(store.step_memory().list_empty_file_sessions(&at).unwrap(), [EmptyFileSession::Abandoned { address: address("former") }], "told from the item's before any write");
    assert_eq!(abandoned(&mut store), []);

    let made = make_file(&mut store, ROOT, "report");
    store.step_memory().remember_upload_session_opening(made, &at, 50).unwrap();
    assert_eq!((kept(&store, former).upload_session, abandoned(&mut store)), (None, vec![address("former")]));
    store.step_memory().remember_upload_session_opening(made, &place(ROOT, "other"), 60).unwrap();
    assert_eq!((said(&mut store, live).upload_session, abandoned(&mut store)), (Some(of_live), vec![address("former")]), "a session its row still says stays, though the file was renamed");
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(ROOT, "OTHER")).unwrap(), [EmptyFileSession::Continued { item: live, address: address("live"), expires_at: Some(900) }]);
}

/// A session answered as abandoned at a place before any write queued its cancel is done
/// with once it was cancelled: it is not left in its item's row to be cancelled again.
#[test]
fn a_cancelled_session_is_not_left_in_the_row_that_does_not_say_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let (stale, live) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let (of_stale, of_live) = (new_file_session(&mut store, stale, "stale"), new_file_session(&mut store, live, "live"));
    assert_eq!(remember_session(&mut store, stale, &of_stale), Continued);
    assert_eq!(remember_session(&mut store, live, &of_live), Continued);
    change_content(&mut store, stale);
    store.step_memory().forget_upload_session_cancellation(&address("stale")).unwrap();
    assert_eq!((kept(&store, stale).upload_session, abandoned(&mut store)), (None, vec![]));
    store.step_memory().forget_upload_session_cancellation(&address("live")).unwrap();
    assert_eq!(said(&mut store, live).upload_session, Some(of_live), "a session its row says is not a cancellation");
}

/// An empty file is at a place only while the directory of that place has a row: a session
/// given with a place in a directory that went is kept without it.
#[test]
fn a_session_is_kept_without_a_place_in_a_directory_that_went() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let session = UploadSession { empty_file_place: Some(place(LocalItemId::new(99), "report")), ..new_file_session(&mut store, file, "1") };
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    assert_eq!(said(&mut store, file).upload_session, Some(UploadSession { empty_file_place: None, ..session }));
}

/// The item an upload left with other content is left to remove also when the file's row
/// went first: it is kept past the row, read first left first, and forgotten once removed.
#[test]
fn the_item_an_upload_left_is_kept_to_be_removed_when_its_files_row_goes() {
    let mut store = TreeStore::in_memory().unwrap();
    let (first, second, kept_file) = (make_file(&mut store, ROOT, "first"), make_file(&mut store, ROOT, "second"), make_file(&mut store, ROOT, "kept"));
    let left = |id: &str| MismatchedItem::of_answer(reference(id), Some(&format!("c-{id}")), None);
    for (file, id) in [(first, "B1"), (second, "B2"), (kept_file, "B3")] {
        assert_eq!(store.step_memory().remember_mismatched_item(file, &left(id)).unwrap(), LeftToRemove);
    }
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [], "each is its file's while the row is there");
    remove(&mut store, second);
    remove(&mut store, first);
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [left("B2"), left("B1")]);
    assert_eq!(store.step_memory().list_mismatched_item_removals(1).unwrap(), [left("B2")]);
    store.step_memory().forget_mismatched_item(&reference("B2")).unwrap();
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [left("B1")]);
    assert_eq!(said(&mut store, kept_file).mismatched_item, Some(left("B3")));
}

/// A row that goes takes what other rows held of a place in it: ids are never given twice,
/// so no place will be in it again.
#[test]
fn what_was_learnt_of_a_place_in_a_directory_goes_with_the_directorys_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    let file = make_file(&mut store, directory, "report");
    let (refusal, taken) = (refusal_of(&mut store, file, Reason::Forbidden), taken_at(&mut store, file, "REPORT"));
    assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
    assert_eq!(remember_taken(&mut store, file, &taken), StillSaid);
    let _ = remember_try(&mut store, file, &Reason::Network, None);
    rename(&mut store, file, "report");
    remove(&mut store, directory);
    let after = kept(&store, file);
    assert_eq!((after.refusal, after.taken_name, after.failed_tries.map(|tries| tries.count)), (None, None, Some(1)));
}
