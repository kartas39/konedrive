//! The sequences in which a fact of one unsent change was answered for an item nothing was
//! unsent of, or for the next unsent change: each as it was found, through the store.

use std::ffi::OsStr;

use konedrive_reason::Reason;

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, listed_gone, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::{ConfirmedItem, StepCommitOutcome};
use crate::sync_plan::ItemVersions;

fn versions(store: &mut TreeStore, id: LocalItemId) -> ItemVersions {
    ItemVersions::of(&item(store, id))
}

fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let id = store.item_store().find_bound_item(&reference(item_id)).unwrap().expect("listed").local_item_id;
    let planned_from = versions(store, id);
    assert_eq!(store.item_store().commit_show_item(&planned_from, false).unwrap(), StepCommitOutcome::Written);
    id
}

/// A try of the step planned from `planned_from` failed.
fn remember_try_of(store: &mut TreeStore, planned_from: &ItemVersions, next_try_at: Option<i64>) -> Option<u32> {
    store.step_memory().remember_failed_try(planned_from.local_item_id, TriedChange::of_versions(planned_from).as_ref(), &Reason::Network, next_try_at).unwrap()
}

/// A store whose folder shows the directory `docs` of OneDrive, `D`.
fn store_with_docs() -> (TreeStore, LocalItemId) {
    let mut store = store_with_listed_root();
    write_listing_of_changes(&mut store, &[listed_directory("D", "R", "docs", "e-D")]);
    let docs = show(&mut store, "D");
    (store, docs)
}

/// A taken name says that another item of OneDrive has the name. A new file is answered
/// that its name is taken; a listing brings the holder, and the plan merges the two; the
/// holder is removed in OneDrive and the file, changed here, gives its binding up to be
/// sent again under its own name: no name is answered as taken for it.
#[test]
fn a_taken_name_is_not_answered_once_the_item_is_the_one_that_holds_the_name() {
    let (mut store, docs) = store_with_docs();
    let file = make_file(&mut store, docs, "report");
    let taken = TakenName { place: place(docs, "report"), holder_name: "Report".into() };
    assert_eq!(remember_taken(&mut store, file, &taken), StillSaid);
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (0, "h-N"), "e-N")]);
    let holder = store.item_store().find_bound_item(&reference("N")).unwrap().unwrap().local_item_id;
    let (local, onedrive) = (versions(&mut store, file), versions(&mut store, holder));
    assert_eq!(store.item_store().commit_merge_items(&local, &onedrive).unwrap(), StepCommitOutcome::Written);
    assert_eq!((said(&mut store, file).taken_name, kept(&store, file)), (None, ItemMemory::default()), "the item is bound to the holder");

    change_content(&mut store, file);
    write_listing_of_changes(&mut store, &[listed_gone("N")]);
    let planned_from = versions(&mut store, file);
    assert_eq!(store.item_store().commit_give_up_binding(&planned_from, false).unwrap(), StepCommitOutcome::Written);
    store.step_memory().remember_upload_session_opening(file, &place(docs, "report"), 10).unwrap();
    assert_eq!(store.step_memory().list_taken_names(docs).unwrap(), [], "sent again as a new file under its own name");
}

/// A refusal for good is of what is unsent. An item moved here is refused; another client
/// moves it to the same place, and the two sides agree: nothing is unsent of it, and it is
/// neither listed nor counted as refused.
#[test]
fn a_refusal_is_not_answered_for_an_item_nothing_is_unsent_of() {
    let (mut store, docs) = store_with_docs();
    write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (3, "h-X"), "e-X")]);
    let file = show(&mut store, "X");
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(docs, "same"), is_name_ignored: false }).unwrap();
    let refusal = refusal_of(&mut store, file, Reason::Forbidden);
    assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
    assert_eq!(store.step_memory().count_refusal_reasons(Some(&Reason::Forbidden)).unwrap(), 1);

    write_listing_of_changes(&mut store, &[listed_file("X", "D", "same", (3, "h-X"), "e-X2")]);
    let planned_from = versions(&mut store, file);
    assert_eq!(store.item_store().commit_agree_on_place(&planned_from).unwrap(), StepCommitOutcome::Written);
    assert_eq!(item(&mut store, file).unsent_since, None);
    assert_eq!((store.step_memory().list_refusal_reasons(None, None, 10).unwrap(), store.step_memory().count_refusal_reasons(Some(&Reason::Forbidden)).unwrap()), (vec![], 0));
    assert_eq!(remember_refused(&mut store, file, &refusal), VersionGone, "nor written for a row with nothing unsent");
    assert_eq!(store.step_memory().remember_refusal(file, planned_from.unsent_since, &refusal).unwrap(), VersionGone, "nor with the stamp it had");
}

/// The failed tries are of one unsent change. An item renamed here fails to be moved; the
/// user renames it back, and nothing is unsent: it does not wait until a time, though no
/// commit ever ran for it. It is edited later, and the first failure of that upload is the
/// first.
#[test]
fn the_tries_of_an_unsent_change_are_not_answered_or_counted_on_for_the_next() {
    let (mut store, docs) = store_with_docs();
    write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (3, "h-X"), "e-X")]);
    let file = show(&mut store, "X");
    let of_no_unsent_change = TriedChange::Unsent { unsent_since: 1 };
    assert_eq!(store.step_memory().remember_failed_try(file, Some(&of_no_unsent_change), &Reason::Network, Some(60)).unwrap(), None, "nothing is unsent of it: no try of an unsent change is counted");
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(docs, "renamed"), is_name_ignored: false }).unwrap();
    assert_eq!(remember_try(&mut store, file, &Reason::Network, Some(60)), Some(1));
    assert_eq!(remember_try(&mut store, file, &Reason::Network, Some(120)), Some(2));
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), [TimedWait { item: file, until: 120 }]);

    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(docs, "report"), is_name_ignored: false }).unwrap();
    assert_eq!((item(&mut store, file).unsent_since, store.step_memory().list_timed_waits(10).unwrap(), said(&mut store, file).failed_tries), (None, vec![], None));

    change_content(&mut store, file);
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), [], "unsent again: another change");
    assert_eq!(remember_try(&mut store, file, &Reason::Network, Some(300)), Some(1), "counted from one");
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), [TimedWait { item: file, until: 300 }]);
}

/// The upload session a commit is told ended is forgotten and never cancelled, also when
/// the step was planned from another content than the session sends; a session the commit
/// is not told of is not answered for an item nothing is unsent of, and its cancel is left.
#[test]
fn the_session_a_commit_is_told_ended_is_forgotten_and_another_is_cancelled() {
    for is_told in [true, false] {
        let (mut store, _docs) = store_with_docs();
        write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (3, "h-X"), "e-X")]);
        let file = show(&mut store, "X");
        change_content(&mut store, file);
        let session = UploadSession { address: address("1"), expires_at: None, next_offset: 0, content: local_content(&mut store, file), empty_file_place: None };
        assert_eq!(remember_session(&mut store, file, &session), Continued);
        // A writer opens the file and the daemon stops: no close is written, and the plan
        // sends the content the row has, which is not the session's.
        store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
        assert!(said(&mut store, file).upload_session.is_some(), "what it was before the writer is what the session sends");
        let planned_from = versions(&mut store, file);
        let content = ContentVersion::Hashed { size: 77, mtime: 1000, mtime_nsec: 0, hash: Some("h-sent".to_owned()), ctag: Some("c-h-sent".to_owned()) };
        let sent = ConfirmedItem { item: reference("X"), parent: reference("D"), name: "report".into(), content: Some(content), etag: "e-X2".to_owned() };
        let ended = address("1");
        assert_eq!(store.item_store().commit_send_content(&planned_from, &sent, is_told.then_some(&ended)).unwrap(), StepCommitOutcome::Written);
        assert_eq!((item(&mut store, file).unsent_since, said(&mut store, file).upload_session), (None, None), "told: {is_told}");
        store.step_memory().forget_fact(file, StepFact::TakenName).unwrap();
        assert_eq!((kept(&store, file), abandoned(&mut store)), (ItemMemory::default(), if is_told { vec![] } else { vec![address("1")] }));
    }
}

/// A new file renamed while its upload session runs: the upload ends under the name of the
/// session's empty file, and its commit is given that name. The place agreed on is then the
/// one OneDrive has, and the rename is unsent, to be sent as a move.
#[test]
fn a_new_file_renamed_during_its_upload_is_committed_under_its_sessions_name() {
    let (mut store, docs) = store_with_docs();
    let file = make_file(&mut store, docs, "report");
    let session = new_file_session(&mut store, file, "1");
    assert_eq!(remember_session(&mut store, file, &session), Continued);
    let planned_from = versions(&mut store, file);
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(docs, "final"), is_name_ignored: false }).unwrap();
    let session = said(&mut store, file).upload_session.expect("a rename leaves the bytes");
    let at = session.empty_file_place.expect("of a new file");
    let content = ContentVersion::Hashed { size: 0, mtime: 200, mtime_nsec: 0, hash: Some("h-new".to_owned()), ctag: Some("c-new".to_owned()) };
    let made = ConfirmedItem { item: reference("NEW"), parent: reference("D"), name: at.name.clone(), content: Some(content), etag: "e-NEW".to_owned() };
    assert_eq!(store.item_store().commit_send_new_file(&planned_from, &reference("D"), OsStr::new(&at.name), &made, Some(&session.address)).unwrap(), StepCommitOutcome::Written);
    let row = item(&mut store, file);
    assert_eq!((row.local.unwrap().place, row.onedrive.unwrap().place, row.synced_place), (Some(place(docs, "final")), Some(at.clone()), Some(at)));
    assert!(row.unsent_since.is_some(), "the rename is still to be sent");
    assert_eq!((kept(&store, file), abandoned(&mut store)), (ItemMemory::default(), vec![]));
}

/// Nothing of the root is ever unsent, so no fact of an unsent change is learnt of it,
/// with whatever stamp a caller gives; and an item nothing is unsent of learns none either.
#[test]
fn nothing_of_an_unsent_change_is_learnt_of_the_root_or_of_an_item_the_two_sides_agree_on() {
    let (mut store, docs) = store_with_docs();
    assert_eq!((item(&mut store, ROOT).unsent_since, item(&mut store, docs).unsent_since), (None, None));
    for id in [ROOT, docs] {
        for unsent_since in [None, Some(1)] {
            let session = UploadSession { address: address("1"), expires_at: None, next_offset: 0, content: ContentVersion::ChangedHere { size: 0, mtime: 0, mtime_nsec: 0, change_count: 1 }, empty_file_place: None };
            let taken = TakenName { place: place(ROOT, "docs"), holder_name: "Docs".into() };
            let mut memory = store.step_memory();
            assert_eq!(memory.remember_upload_session(id, unsent_since, &session).unwrap(), Abandoned);
            assert_eq!(memory.remember_failed_try(id, unsent_since.map(|unsent_since| TriedChange::Unsent { unsent_since }).as_ref(), &Reason::Network, Some(60)).unwrap(), None);
            assert_eq!(memory.remember_failed_try(id, Some(&TriedChange::OfOneDrive { etag: "e-none".to_owned() }), &Reason::Network, Some(60)).unwrap(), None);
            assert_eq!(memory.remember_taken_name(id, unsent_since, &taken).unwrap(), VersionGone);
            assert_eq!(memory.remember_removals_confirmed(&[UnsentRemoval { item: id, unsent_since: 1 }]).unwrap(), 0);
            memory.forget_upload_session_cancellation(&address("1")).unwrap();
        }
        assert_eq!((kept(&store, id), store.step_memory().list_timed_waits(10).unwrap()), (ItemMemory::default(), vec![]));
    }
}

/// A step towards the folder of an item nothing is unsent of fails: its tries are of what
/// OneDrive has, told by the eTag the step was planned from. It waits, the count goes on,
/// and it is over once the step is written, or OneDrive has another version.
#[test]
fn the_tries_of_a_step_for_an_item_nothing_is_unsent_of_are_of_onedrives_version() {
    let (mut store, _docs) = store_with_docs();
    write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (3, "h-X"), "e-X")]);
    let file = show(&mut store, "X");
    write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (9, "h-X2"), "e-X2")]);
    let planned_from = versions(&mut store, file);
    assert_eq!(planned_from.unsent_since, None, "OneDrive changed the content, and nothing is unsent");
    assert_eq!(remember_try_of(&mut store, &planned_from, Some(60)), Some(1), "the download failed");
    assert_eq!(remember_try_of(&mut store, &planned_from, Some(120)), Some(2));
    assert_eq!(store.step_memory().list_timed_waits(10).unwrap(), [TimedWait { item: file, until: 120 }], "not planned again at once");

    write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (11, "h-X3"), "e-X3")]);
    assert_eq!((store.step_memory().list_timed_waits(10).unwrap(), said(&mut store, file).failed_tries), (vec![], None), "another version of OneDrive: another piece of work");
    assert_eq!(remember_try_of(&mut store, &planned_from, Some(300)), None, "the step was planned from the version before");
    let planned_from = versions(&mut store, file);
    assert_eq!(remember_try_of(&mut store, &planned_from, Some(300)), Some(1), "counted from one");
    assert_eq!(store.item_store().commit_take_onedrive_content(&planned_from).unwrap(), StepCommitOutcome::Written);
    assert_eq!((store.step_memory().list_timed_waits(10).unwrap(), kept(&store, file)), (vec![], ItemMemory::default()), "the step went through");
}
