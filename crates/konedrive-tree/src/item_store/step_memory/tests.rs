use konedrive_reason::Reason;

use super::memory_row::read_memory;
use super::*;
use crate::item_store::tests::{is_refused, item, make_directory, make_file, place, synced};
use crate::model::{ContentVersion, ItemVersion, LocalVersionChange, OneDriveItemId};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// What becomes of each fact when a binding moves.
mod binding_moves;

use FactStanding::{StillSaid, VersionGone};

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
    UploadSession { address: address(of), expires_at: Some(900), next_offset: 0, content: local_content(store, id), placeholder: Some(local_place(store, id)) }
}

fn refusal_of(store: &mut TreeStore, id: LocalItemId, reason: Reason) -> Refusal {
    Refusal { reason, version: Some(local_version(store, id)) }
}

fn taken_at(store: &mut TreeStore, id: LocalItemId, holder_name: &str) -> TakenName {
    TakenName { place: local_place(store, id), holder_name: holder_name.into() }
}

fn temporary_toward(store: &mut TreeStore, id: LocalItemId) -> TemporaryName {
    TemporaryName { name: format!(".konedrive-swap-{id}").into(), toward: local_place(store, id) }
}

fn too_big(store: &mut TreeStore, id: LocalItemId, free_space: u64) -> TooBigForSpace {
    TooBigForSpace { content: local_content(store, id), free_space }
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

fn abandoned(store: &mut TreeStore) -> Vec<UploadSessionAddress> {
    store.step_memory().list_abandoned_upload_sessions(100).unwrap()
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
        upload_session: Some(new_file_session(store, file, "1")),
        mismatched_upload: Some(MismatchedUpload::of_answer(OneDriveItemId::new("B"), Some("c-B"), None)),
        temporary_name: Some(temporary_toward(store, file)),
        failed_tries: Some(FailedTries { count: 1, next_try_at: Some(60), last_failure: Reason::Refused(Some("no".to_owned())) }),
        too_big_for_space: Some(too_big(store, file, 5)),
        is_removal_confirmed: false,
        refusal: Some(refusal_of(store, file, Reason::Forbidden)),
        taken_name: Some(taken_at(store, file, "REPORT")),
    };
    let mut memory = store.step_memory();
    assert_eq!(memory.remember_upload_session(file, expected.upload_session.as_ref().unwrap()).unwrap(), StillSaid);
    memory.remember_mismatched_upload(file, expected.mismatched_upload.as_ref().unwrap()).unwrap();
    assert_eq!(memory.remember_temporary_name(file, expected.temporary_name.as_ref().unwrap()).unwrap(), StillSaid);
    assert_eq!(memory.remember_failed_try(file, &Reason::Refused(Some("no".to_owned())), Some(60)).unwrap(), 1);
    assert_eq!(memory.remember_too_big_for_space(file, expected.too_big_for_space.as_ref().unwrap()).unwrap(), StillSaid);
    assert_eq!(memory.remember_refusal(file, expected.refusal.as_ref().unwrap()).unwrap(), StillSaid);
    assert_eq!(memory.remember_taken_name(file, expected.taken_name.as_ref().unwrap()).unwrap(), StillSaid);
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
    synced(&mut store, file, "A");
    let holder_name = std::ffi::OsString::from_vec(vec![b'R', 0xff, b'T']);
    let taken = TakenName { place: local_place(&mut store, file), holder_name };
    let session = UploadSession { address: address("1"), expires_at: None, next_offset: 5, content: local_content(&mut store, file), placeholder: None };
    assert_eq!(store.step_memory().remember_taken_name(file, &taken).unwrap(), StillSaid);
    assert_eq!(store.step_memory().remember_upload_session(file, &session).unwrap(), StillSaid);
    assert_eq!(said(&mut store, file), ItemMemory { taken_name: Some(taken), upload_session: Some(session), ..ItemMemory::default() });
}

/// Each fact is forgotten by itself, the others staying; with the last the item's row of the
/// table goes. A session forgotten has ended: it is not among the abandoned.
#[test]
fn each_fact_is_forgotten_by_itself_and_the_last_takes_the_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let mut expected = remember_everything_of_a_shown_file(&mut store, file);
    let steps: [(StepFact, fn(&mut ItemMemory)); 7] = [
        (StepFact::UploadSession, |memory| memory.upload_session = None),
        (StepFact::MismatchedUpload, |memory| memory.mismatched_upload = None),
        (StepFact::TemporaryName, |memory| memory.temporary_name = None),
        (StepFact::FailedTries, |memory| memory.failed_tries = None),
        (StepFact::TooBigForSpace, |memory| memory.too_big_for_space = None),
        (StepFact::Refusal, |memory| memory.refusal = None),
        (StepFact::TakenName, |memory| memory.taken_name = None),
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
    let (session, refusal, gone) = (new_file_session(&mut store, file, "1"), refusal_of(&mut store, file, Reason::Forbidden), LocalItemId::new(99));
    let mut memory = store.step_memory();
    assert!(is_refused(memory.remember_upload_session(gone, &session)));
    assert!(is_refused(memory.remember_failed_try(gone, &Reason::Network, None)));
    assert!(is_refused(memory.remember_refusal(gone, &refusal)));
    assert!(is_refused(memory.remember_refusal(ROOT, &refusal)));
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
    store.step_memory().remember_upload_session(file, &session).unwrap();
    store.step_memory().remember_temporary_name(file, &temporary).unwrap();
    store.step_memory().remember_upload_session_opening(other, &place(ROOT, "other"), 10).unwrap();
    // The daemon dies: nothing is closed.
    std::mem::forget(store);

    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!(said(&mut store, file), ItemMemory { upload_session: Some(session), temporary_name: Some(temporary), ..ItemMemory::default() });
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
    assert_eq!(store.step_memory().list_placeholder_sessions(&place(ROOT, "report")).unwrap(), [PlaceholderSession::Abandoned { address: address("1") }]);

    // And by a statement no operation of the store makes.
    let second = make_file(&mut store, ROOT, "second");
    store.step_memory().remember_failed_try(second, &Reason::Network, Some(5)).unwrap();
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
/// abandoned ones.
#[test]
fn what_was_learnt_of_a_content_is_not_answered_once_the_content_changed() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (session, big) = (new_file_session(&mut store, file, "1"), too_big(&mut store, file, 5));
    store.step_memory().remember_upload_session(file, &session).unwrap();
    store.step_memory().remember_too_big_for_space(file, &big).unwrap();

    change_content(&mut store, file);
    assert_eq!(said(&mut store, file), ItemMemory::default(), "not answered");
    assert_eq!(store.step_memory().list_user_reasons(None, 10).unwrap(), [], "no reason for the user");
    assert_eq!(kept(&store, file).upload_session, Some(session.clone()), "kept until the next write");
    assert_eq!(store.step_memory().advance_upload_session(file, &session.address, 9, None).unwrap(), VersionGone, "not gone on with");

    store.step_memory().remember_failed_try(file, &Reason::Network, None).unwrap();
    let after = kept(&store, file);
    assert_eq!((after.upload_session, after.too_big_for_space, after.failed_tries.map(|tries| tries.count)), (None, None, Some(1)));
    assert_eq!(abandoned(&mut store), [address("1")]);
}

/// A refusal, a temporary name and a taken name are not answered once the item's place
/// changed, and go with the next write for the item; the failed tries hold no version and
/// stay.
#[test]
fn what_was_learnt_of_a_place_is_not_answered_once_the_place_changed() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (refusal, temporary, taken) = (refusal_of(&mut store, file, Reason::Forbidden), temporary_toward(&mut store, file), taken_at(&mut store, file, "REPORT"));
    store.step_memory().remember_refusal(file, &refusal).unwrap();
    store.step_memory().remember_temporary_name(file, &temporary).unwrap();
    store.step_memory().remember_taken_name(file, &taken).unwrap();
    store.step_memory().remember_failed_try(file, &Reason::Locked, Some(40)).unwrap();
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), [(file, taken)]);

    rename(&mut store, file, "renamed");
    let tries = Some(FailedTries { count: 1, next_try_at: Some(40), last_failure: Reason::Locked });
    assert_eq!(said(&mut store, file), ItemMemory { failed_tries: tries.clone(), ..ItemMemory::default() });
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), []);
    assert_eq!(store.step_memory().list_user_reasons(None, 10).unwrap(), []);
    assert!(kept(&store, file).refusal.is_some(), "kept until the next write");

    store.step_memory().forget_fact(file, StepFact::MismatchedUpload).unwrap();
    assert_eq!(kept(&store, file), ItemMemory { failed_tries: tries, ..ItemMemory::default() });
}

/// A fact given when the row does not say it any more is not written, and the writer says
/// so; an upload session given so is abandoned at once.
#[test]
fn a_fact_the_row_does_not_say_any_more_is_not_written() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let (session, big, refusal) = (new_file_session(&mut store, file, "1"), too_big(&mut store, file, 5), refusal_of(&mut store, file, Reason::Forbidden));
    let (temporary, taken) = (temporary_toward(&mut store, file), taken_at(&mut store, file, "REPORT"));
    change_content(&mut store, file);
    rename(&mut store, file, "renamed");
    let mut memory = store.step_memory();
    assert_eq!(memory.remember_upload_session(file, &session).unwrap(), VersionGone);
    assert_eq!(memory.remember_too_big_for_space(file, &big).unwrap(), VersionGone);
    assert_eq!(memory.remember_refusal(file, &refusal).unwrap(), VersionGone);
    assert_eq!(memory.remember_temporary_name(file, &temporary).unwrap(), VersionGone);
    assert_eq!(memory.remember_taken_name(file, &taken).unwrap(), VersionGone);
    assert_eq!((count(&store, "step_memory"), abandoned(&mut store)), (0, vec![address("1")]));
}

/// The upload session: how far it got is written while the address is the item's; another
/// session abandons the earlier one; one given up is abandoned until it is cancelled.
#[test]
fn an_upload_session_advances_is_replaced_and_is_abandoned() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let first = new_file_session(&mut store, file, "1");
    store.step_memory().remember_upload_session(file, &first).unwrap();
    assert_eq!(store.step_memory().advance_upload_session(file, &first.address, 4096, Some(950)).unwrap(), StillSaid);
    assert_eq!(said(&mut store, file).upload_session, Some(UploadSession { next_offset: 4096, expires_at: Some(950), ..first.clone() }));
    assert_eq!(store.step_memory().advance_upload_session(file, &address("other"), 1, None).unwrap(), VersionGone);
    assert_eq!(store.step_memory().list_placeholder_sessions(&place(ROOT, "REPORT")).unwrap(), [PlaceholderSession::OfItem { item: file, address: address("1") }]);

    let second = new_file_session(&mut store, file, "2");
    store.step_memory().remember_upload_session(file, &second).unwrap();
    assert_eq!((said(&mut store, file).upload_session, abandoned(&mut store)), (Some(second), vec![address("1")]));

    store.step_memory().abandon_upload_session(file).unwrap();
    assert_eq!((said(&mut store, file), abandoned(&mut store)), (ItemMemory::default(), vec![address("1"), address("2")]), "the one abandoned first first");
    assert_eq!(store.step_memory().list_abandoned_upload_sessions(1).unwrap(), [address("1")]);
    store.step_memory().forget_abandoned_upload_session(&address("1")).unwrap();
    assert_eq!(abandoned(&mut store), [address("2")]);
}

/// The failed tries are counted; the items that wait until a time are read soonest first,
/// at most as many as asked; a refresh makes every one due and keeps count and reason.
#[test]
fn the_tries_are_counted_and_the_items_that_wait_are_read_soonest_first() {
    let mut store = TreeStore::in_memory().unwrap();
    let (a, b, c) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"), make_file(&mut store, ROOT, "c"));
    let mut memory = store.step_memory();
    assert_eq!(memory.remember_failed_try(a, &Reason::Network, Some(300)).unwrap(), 1);
    assert_eq!(memory.remember_failed_try(b, &Reason::Network, Some(100)).unwrap(), 1);
    assert_eq!(memory.remember_failed_try(b, &Reason::Locked, Some(200)).unwrap(), 2);
    assert_eq!(memory.remember_failed_try(c, &Reason::Network, None).unwrap(), 1);
    assert_eq!(memory.list_timed_waits(10).unwrap(), [TimedWait { item: b, until: 200 }, TimedWait { item: a, until: 300 }], "one due at once does not wait");
    assert_eq!(memory.list_timed_waits(1).unwrap(), [TimedWait { item: b, until: 200 }]);
    assert_eq!(said(&mut store, b).failed_tries, Some(FailedTries { count: 2, next_try_at: Some(200), last_failure: Reason::Locked }));

    assert_eq!(store.step_memory().clear_next_tries().unwrap(), 2);
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), []);
    assert_eq!(said(&mut store, b).failed_tries, Some(FailedTries { count: 2, next_try_at: None, last_failure: Reason::Locked }));
    store.step_memory().forget_fact(b, StepFact::FailedTries).unwrap();
    assert_eq!(store.step_memory().remember_failed_try(b, &Reason::Network, Some(1)).unwrap(), 1, "counted anew after a step went through");
}

/// A quota read lets go every file that fits in the free space and says the free space
/// again of the rest.
#[test]
fn a_quota_read_lets_go_what_fits_and_says_the_free_space_of_the_rest() {
    let mut store = TreeStore::in_memory().unwrap();
    let (small, large) = (make_file(&mut store, ROOT, "small"), make_file(&mut store, ROOT, "large"));
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: small, size: 10, mtime: 300, mtime_nsec: 0 }).unwrap();
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: large, size: 1000, mtime: 300, mtime_nsec: 0 }).unwrap();
    let (small_fact, large_fact) = (too_big(&mut store, small, 5), too_big(&mut store, large, 5));
    store.step_memory().remember_too_big_for_space(small, &small_fact).unwrap();
    store.step_memory().remember_too_big_for_space(large, &large_fact).unwrap();
    assert_eq!(
        store.step_memory().list_user_reasons(None, 10).unwrap(),
        [UserReason { item: small, reason: Reason::TooBig(Some((10, 5))) }, UserReason { item: large, reason: Reason::TooBig(Some((1000, 5))) }]
    );

    assert_eq!(store.step_memory().write_free_space(500).unwrap(), 1);
    assert_eq!(said(&mut store, small), ItemMemory::default());
    assert_eq!(said(&mut store, large).too_big_for_space, Some(TooBigForSpace { free_space: 500, ..large_fact }));
}

/// The confirmation of a mass removal is written of the items whose row is a removal not
/// sent, and of no other; it is answered while the row is one.
#[test]
fn a_confirmation_is_written_of_a_removal_that_is_not_sent() {
    let mut store = TreeStore::in_memory().unwrap();
    let (removed, shown, never_sent) = (make_file(&mut store, ROOT, "removed"), make_file(&mut store, ROOT, "shown"), make_file(&mut store, ROOT, "never-sent"));
    synced(&mut store, removed, "A");
    synced(&mut store, shown, "B");
    remove(&mut store, removed);
    assert_eq!(store.step_memory().remember_removals_confirmed(&[removed, shown, never_sent, LocalItemId::new(99)]).unwrap(), 1);
    assert!(said(&mut store, removed).is_removal_confirmed);
    assert_eq!((said(&mut store, shown), said(&mut store, never_sent)), (ItemMemory::default(), ItemMemory::default()));
    store.step_memory().forget_fact(removed, StepFact::RemovalConfirmation).unwrap();
    assert_eq!((said(&mut store, removed), count(&store, "step_memory")), (ItemMemory::default(), 0));
}

/// The items with a reason for the user are read by local item id, a bounded number after
/// the item given; a refusal comes before a file too big, and a fact the row does not say
/// any more is left out. The refusals of one reason are forgotten together.
#[test]
fn the_items_with_a_reason_for_the_user_are_read_bounded() {
    let mut store = TreeStore::in_memory().unwrap();
    let files: Vec<LocalItemId> = (0..5).map(|n| make_file(&mut store, ROOT, &format!("file-{n}"))).collect();
    for (file, reason) in [(files[0], Reason::Forbidden), (files[1], Reason::Refused(Some("bad name".to_owned()))), (files[2], Reason::Forbidden), (files[4], Reason::Forbidden)] {
        let refusal = refusal_of(&mut store, file, reason);
        store.step_memory().remember_refusal(file, &refusal).unwrap();
    }
    let big = too_big(&mut store, files[0], 5);
    store.step_memory().remember_too_big_for_space(files[0], &big).unwrap();
    rename(&mut store, files[2], "renamed");

    let reason = |item, reason| UserReason { item, reason };
    let all = [reason(files[0], Reason::Forbidden), reason(files[1], Reason::Refused(Some("bad name".to_owned()))), reason(files[4], Reason::Forbidden)];
    assert_eq!(store.step_memory().list_user_reasons(None, 10).unwrap(), all);
    assert_eq!(store.step_memory().list_user_reasons(None, 2).unwrap(), all[..2]);
    assert_eq!(store.step_memory().list_user_reasons(Some(files[0]), 1).unwrap(), all[1..2]);
    assert_eq!(store.step_memory().list_user_reasons(Some(files[1]), 1).unwrap(), all[2..], "what the row does not say is passed over");

    assert_eq!(store.step_memory().forget_refusals_for(&Reason::Forbidden).unwrap(), 3, "also the one its row does not say any more");
    assert_eq!(
        store.step_memory().list_user_reasons(None, 10).unwrap(),
        [reason(files[0], Reason::TooBig(Some((0, 5)))), reason(files[1], Reason::Refused(Some("bad name".to_owned())))]
    );
}

/// The names OneDrive answered are taken are read for one directory, each with its item.
#[test]
fn the_taken_names_are_read_for_one_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    let (inside, other_inside, outside) = (make_file(&mut store, directory, "straße"), make_file(&mut store, directory, "plain"), make_file(&mut store, ROOT, "top"));
    let (taken_inside, taken_outside) = (taken_at(&mut store, inside, "STRASSE"), taken_at(&mut store, outside, "TOP"));
    store.step_memory().remember_taken_name(inside, &taken_inside).unwrap();
    store.step_memory().remember_taken_name(outside, &taken_outside).unwrap();
    store.step_memory().remember_failed_try(other_inside, &Reason::Network, None).unwrap();
    assert_eq!(store.step_memory().list_taken_names(directory).unwrap(), [(inside, taken_inside)]);
    assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap(), [(outside, taken_outside)]);
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
    assert_eq!(memory.remember_upload_session_opening(file, &at, 10).unwrap(), OpeningRecord::Made);
    let carried = memory.remember_upload_session_opening(file, &place(ROOT, "Report"), 50).unwrap();
    assert_eq!(carried, OpeningRecord::Carried { last_unknown_at: 10 });
    assert_eq!(windows(&mut store, &at), [(10, 50)]);
    store.step_memory().settle_answered_opening(file, carried).unwrap();
    assert_eq!(windows(&mut store, &at), [(10, 10)], "the earlier try is still not answered");

    assert_eq!(store.step_memory().remember_upload_session_opening(file, &place(ROOT, "elsewhere"), 70).unwrap(), OpeningRecord::Made);
    assert_eq!((windows(&mut store, &at), windows(&mut store, &place(ROOT, "elsewhere"))), (vec![(10, 10)], vec![(70, 70)]));
    store.step_memory().settle_answered_opening(file, OpeningRecord::Made).unwrap();
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
    store.step_memory().remember_upload_session(file, &session).unwrap();
    assert_eq!((count(&store, "upload_session_openings"), said(&mut store, file).upload_session), (0, Some(session)));
}
