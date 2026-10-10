use konedrive_reason::Reason;

use super::*;
use crate::item_store::bindings::take_over_binding;
use crate::item_store::tests::reference;
use crate::model::ItemKind;

fn sorted(mut addresses: Vec<UploadSessionAddress>) -> Vec<String> {
    addresses.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    addresses.into_iter().map(|address| address.as_str().to_owned()).collect()
}

/// `take_over_binding`: the confirmation of the replaced file's removal is of that removal,
/// and goes with its row: the file that was made in its place and is removed later is
/// another removal, which nobody confirmed. The replacing file's upload session, temporary
/// name, refusal and failed tries are dropped, the session to the cancellations with the one
/// the replaced row had; the item an upload left, the refusal for lack of space and the
/// taken name stay with it.
#[test]
fn a_binding_taken_over_drops_the_confirmation_and_what_was_asked_of_onedrive() {
    let mut store = TreeStore::in_memory().unwrap();
    let replaced = make_file(&mut store, ROOT, "report");
    synced(&mut store, replaced, "A");
    let update = UploadSession { address: address("replaced"), expires_at: None, next_offset: 0, content: local_content(&mut store, replaced), empty_file_place: None };
    store.step_memory().remember_upload_session(replaced, &update).unwrap();
    remove(&mut store, replaced);
    let removal = unsent_removal(&mut store, replaced);
    assert_eq!(store.step_memory().remember_removals_confirmed(&[removal]).unwrap(), 1);
    let replacing = make_file(&mut store, ROOT, "report.tmp");
    let before = remember_everything_of_a_shown_file(&mut store, replacing);

    store.item_store().write(|tx| take_over_binding(tx, replacing, replaced)).unwrap();
    assert_eq!(kept(&store, replacing), ItemMemory { mismatched_item: before.mismatched_item, space_refusal: before.space_refusal, taken_name: before.taken_name, ..ItemMemory::default() });
    assert_eq!((count(&store, "step_memory"), sorted(abandoned(&mut store))), (1, vec!["https://up/1".to_owned(), "https://up/replaced".to_owned()]));
    remove(&mut store, replacing);
    assert_eq!(said(&mut store, replacing).removal_confirmation, None, "removed again: a removal nobody confirmed");
    assert_eq!(store.step_memory().list_items_with_confirmed_removal(None, 10).unwrap(), []);
}

/// `give_up_binding` of a file: it keeps its id and its content, and what was asked of
/// OneDrive for it as the item it was bound to is dropped, its session to the
/// cancellations; a file too big for the space stays too big. The row that takes the binding has
/// nothing.
#[test]
fn a_file_that_gives_its_binding_up_drops_what_was_asked_of_onedrive_for_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    synced(&mut store, file, "A");
    let update = UploadSession { address: address("update"), expires_at: None, next_offset: 7, content: local_content(&mut store, file), empty_file_place: None };
    let (refusal, big) = (refusal_of(&mut store, file, Reason::Forbidden), space_refusal(&mut store, file, 1));
    store.step_memory().remember_upload_session(file, &update).unwrap();
    store.step_memory().remember_refusal(file, &refusal).unwrap();
    store.step_memory().remember_failed_try(file, &Reason::Locked, Some(40)).unwrap();
    store.step_memory().remember_space_refusal(file, &big).unwrap();

    let taker = store.item_store().give_up_binding(file, false).unwrap();
    assert_eq!(kept(&store, file), ItemMemory { space_refusal: Some(big), ..ItemMemory::default() });
    assert_eq!((kept(&store, taker), abandoned(&mut store)), (ItemMemory::default(), vec![address("update")]));
}

/// `give_up_binding` of a directory: what was asked of the directory of OneDrive is with the
/// row that has its binding now — the openings in it, and the empty files of the sessions
/// in it, which are abandoned — and what OneDrive answered about sending an item into it is
/// dropped, a taken name and a refusal: the directory the folder shows is one OneDrive does
/// not have yet. Of a file removed in it, the confirmation and the refusal of the removal
/// stand: it is removed from the same directory of OneDrive.
#[test]
fn a_directory_that_gives_its_binding_up_leaves_what_was_asked_of_it_with_the_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    synced(&mut store, directory, "D");
    let (sending, asking, refused, removed) =
        (make_file(&mut store, directory, "sending"), make_file(&mut store, directory, "asking"), make_file(&mut store, directory, "straße"), make_file(&mut store, directory, "removed"));
    let (session, taken, refusal) = (new_file_session(&mut store, sending, "1"), taken_at(&mut store, refused, "STRASSE"), refusal_of(&mut store, refused, Reason::Forbidden));
    store.step_memory().remember_upload_session(sending, &session).unwrap();
    store.step_memory().remember_upload_session_opening(asking, &place(directory, "asking"), 10).unwrap();
    store.step_memory().remember_taken_name(refused, &taken).unwrap();
    assert_eq!(store.step_memory().remember_refusal(refused, &refusal).unwrap(), StillSaid);
    synced(&mut store, removed, "X");
    remove(&mut store, removed);
    let removal = unsent_removal(&mut store, removed);
    let refused_removal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal { unsent_since: removal.unsent_since } };
    store.step_memory().remember_removals_confirmed(&[removal]).unwrap();
    store.step_memory().remember_refusal(removed, &refused_removal).unwrap();

    let taker = store.item_store().give_up_binding(directory, false).unwrap();
    assert_eq!((windows(&mut store, &place(directory, "asking")), windows(&mut store, &place(taker, "asking"))), (vec![], vec![(10, 10)]));
    assert_eq!((said(&mut store, sending), abandoned(&mut store)), (ItemMemory::default(), vec![address("1")]));
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(taker, "sending")).unwrap(), [EmptyFileSession::Abandoned { address: address("1") }]);
    assert_eq!((store.step_memory().list_taken_names(directory).unwrap(), kept(&store, refused)), (vec![], ItemMemory::default()), "the refusal goes with the taken name");
    assert_eq!(unsent_removal(&mut store, removed), removal, "the same removal, agreed to be in the directory the taker has now");
    assert_eq!(said(&mut store, removed), ItemMemory { removal_confirmation: Some(RemovalConfirmation { unsent_since: removal.unsent_since }), refusal: Some(refused_removal), ..ItemMemory::default() });
}

/// `merge_items`: the local item takes the binding, and what was asked of OneDrive for it as
/// a new item is dropped, its session to the cancellations; the item an upload left stays.
/// The openings in a directory of OneDrive that is merged are in the local directory.
#[test]
fn items_that_are_merged_drop_the_new_items_session_and_keep_the_openings_of_the_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_file(&mut store, ROOT, "report");
    let before = remember_everything_of_a_shown_file(&mut store, local);
    let onedrive = store.item_store().make_onedrive_item(&reference("A"), ItemKind::File).unwrap();
    store.item_store().merge_items(local, onedrive).unwrap();
    assert_eq!(
        kept(&store, local),
        ItemMemory { mismatched_item: before.mismatched_item, space_refusal: before.space_refusal, taken_name: before.taken_name, ..ItemMemory::default() }
    );
    assert_eq!(abandoned(&mut store), [address("1")]);

    let local_directory = make_directory(&mut store, ROOT, "docs");
    let onedrive_directory = store.item_store().make_onedrive_item(&reference("D"), ItemKind::Directory).unwrap();
    store.step_memory().remember_upload_session_opening(local, &place(onedrive_directory, "x"), 10).unwrap();
    store.item_store().merge_items(local_directory, onedrive_directory).unwrap();
    assert_eq!(windows(&mut store, &place(local_directory, "x")), [(10, 10)]);
}
