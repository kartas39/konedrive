use konedrive_reason::Reason;

use super::*;
use crate::item_store::bindings::take_over_binding;
use crate::item_store::tests::reference;
use crate::model::ItemKind;

fn sorted(mut addresses: Vec<UploadSessionAddress>) -> Vec<String> {
    addresses.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    addresses.into_iter().map(|address| address.as_str().to_owned()).collect()
}

/// `take_over_binding`: the confirmation of the replaced file's removal goes to the
/// replacing one with the binding, and is answered when that file is removed again, so that
/// the user is not asked twice. The replacing file's upload session, temporary name, refusal
/// and failed tries are dropped, the session to the abandoned ones with the one the replaced
/// row had; the item an upload left, the file too big and the taken name stay with it.
#[test]
fn a_binding_taken_over_brings_the_confirmation_and_drops_what_was_asked_of_onedrive() {
    let mut store = TreeStore::in_memory().unwrap();
    let replaced = make_file(&mut store, ROOT, "report");
    synced(&mut store, replaced, "A");
    let update = UploadSession { address: address("replaced"), expires_at: None, next_offset: 0, content: local_content(&mut store, replaced), placeholder: None };
    store.step_memory().remember_upload_session(replaced, &update).unwrap();
    remove(&mut store, replaced);
    assert_eq!(store.step_memory().remember_removals_confirmed(&[replaced]).unwrap(), 1);
    let replacing = make_file(&mut store, ROOT, "report.tmp");
    let before = remember_everything_of_a_shown_file(&mut store, replacing);

    store.item_store().write(|tx| take_over_binding(tx, replacing, replaced)).unwrap();
    assert_eq!(
        kept(&store, replacing),
        ItemMemory { mismatched_item: before.mismatched_item, space_refusal: before.space_refusal, taken_name: before.taken_name, is_removal_confirmed: true, ..ItemMemory::default() }
    );
    assert_eq!((count(&store, "step_memory"), sorted(abandoned(&mut store))), (1, vec!["https://up/1".to_owned(), "https://up/replaced".to_owned()]));
    assert!(!said(&mut store, replacing).is_removal_confirmed, "the folder shows it: no removal");
    remove(&mut store, replacing);
    assert!(said(&mut store, replacing).is_removal_confirmed, "removed again: confirmed already");
}

/// `give_up_binding` of a file: it keeps its id and its content, and what was asked of
/// OneDrive for it as the item it was bound to is dropped, its session to the abandoned
/// ones; a file too big for the space stays too big. The row that takes the binding has
/// nothing.
#[test]
fn a_file_that_gives_its_binding_up_drops_what_was_asked_of_onedrive_for_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    synced(&mut store, file, "A");
    let update = UploadSession { address: address("update"), expires_at: None, next_offset: 7, content: local_content(&mut store, file), placeholder: None };
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
/// in it, which are abandoned — and the names OneDrive answered are taken in it are dropped:
/// the directory the folder shows is one OneDrive does not have yet.
#[test]
fn a_directory_that_gives_its_binding_up_leaves_what_was_asked_of_it_with_the_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    synced(&mut store, directory, "D");
    let (sending, asking, refused) = (make_file(&mut store, directory, "sending"), make_file(&mut store, directory, "asking"), make_file(&mut store, directory, "straße"));
    let (session, taken) = (new_file_session(&mut store, sending, "1"), taken_at(&mut store, refused, "STRASSE"));
    store.step_memory().remember_upload_session(sending, &session).unwrap();
    store.step_memory().remember_upload_session_opening(asking, &place(directory, "asking"), 10).unwrap();
    store.step_memory().remember_taken_name(refused, &taken).unwrap();

    let taker = store.item_store().give_up_binding(directory, false).unwrap();
    assert_eq!((windows(&mut store, &place(directory, "asking")), windows(&mut store, &place(taker, "asking"))), (vec![], vec![(10, 10)]));
    assert_eq!((said(&mut store, sending), abandoned(&mut store)), (ItemMemory::default(), vec![address("1")]));
    assert_eq!(store.step_memory().list_placeholder_sessions(&place(taker, "sending")).unwrap(), [PlaceholderSession::Abandoned { address: address("1") }]);
    assert_eq!((store.step_memory().list_taken_names(directory).unwrap(), kept(&store, refused)), (vec![], ItemMemory::default()));
}

/// `merge_items`: the local item takes the binding, and what was asked of OneDrive for it as
/// a new item is dropped, its session to the abandoned ones; the item an upload left stays.
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
