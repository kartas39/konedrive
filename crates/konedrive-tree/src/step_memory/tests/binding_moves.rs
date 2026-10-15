use konedrive_reason::Reason;

use super::*;
use crate::item_store::bindings::take_over_binding;
use crate::item_store::tests::reference;
use crate::model::ItemKind;

fn sorted(mut addresses: Vec<UploadSessionAddress>) -> Vec<String> {
    addresses.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    addresses.into_iter().map(|address| address.as_str().to_owned()).collect()
}

/// `take_over_binding`: every fact of an unsent change of the replacing
/// file is dropped, the taken name among them, its session to the cancellations with the
/// one the replaced row had; the item an upload left is left to the daemon to remove: the
/// file has a binding now, and no step of a new file runs for it. The temporary name the
/// replaced file was under in OneDrive is the replacing file's: it has the binding.
#[test]
fn a_binding_taken_over_drops_what_was_asked_of_onedrive() {
    let mut store = TreeStore::in_memory().unwrap();
    let replaced = make_file(&mut store, ROOT, "report");
    synced(&mut store, replaced, "A");
    change_content(&mut store, replaced);
    let update = UploadSession { address: address("replaced"), expires_at: None, next_offset: 0, content: local_content(&mut store, replaced), empty_file_place: None };
    assert_eq!(remember_session(&mut store, replaced, &update), Continued);
    let under = TemporaryName { name: ".swap".into(), toward: place(ROOT, "elsewhere") };
    store.step_memory().remember_temporary_name(replaced, &under).unwrap();
    remove(&mut store, replaced);
    let replacing = make_file(&mut store, ROOT, "report.tmp");
    let before = remember_everything_of_a_shown_file(&mut store, replacing);

    store.item_store().write(|tx| take_over_binding(tx, replacing, replaced)).unwrap();
    assert_eq!(kept(&store, replacing), ItemMemory { temporary_name: Some(under), ..ItemMemory::default() });
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), before.mismatched_item.into_iter().collect::<Vec<_>>());
    assert_eq!((count(&store, "step_memory"), sorted(abandoned(&mut store))), (1, vec!["https://up/1".to_owned(), "https://up/replaced".to_owned()]));
}

/// `give_up_binding` of a file: it keeps its id and its content, and every fact of an unsent
/// change of it as the item it was bound to is dropped, its session to the cancellations.
/// The row that takes the binding has the temporary name the item is under in OneDrive, and
/// nothing else.
#[test]
fn a_file_that_gives_its_binding_up_drops_what_was_asked_of_onedrive_for_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    synced(&mut store, file, "A");
    change_content(&mut store, file);
    let under = TemporaryName { name: ".swap".into(), toward: place(ROOT, "elsewhere") };
    store.step_memory().remember_temporary_name(file, &under).unwrap();
    let update = UploadSession { address: address("update"), expires_at: None, next_offset: 7, content: local_content(&mut store, file), empty_file_place: None };
    let (refusal, big) = (refusal_of(&mut store, file, Reason::Forbidden), space_refusal(&mut store, file, 1));
    assert_eq!(remember_session(&mut store, file, &update), Continued);
    assert_eq!(remember_refused(&mut store, file, &refusal), StillSaid);
    let _ = remember_try(&mut store, file, &Reason::Locked, Some(40));
    assert_eq!(remember_space(&mut store, file, &big), StillSaid);

    let taker = store.item_store().give_up_binding(file, false).unwrap();
    assert_eq!(kept(&store, file), ItemMemory::default());
    assert_eq!((kept(&store, taker), abandoned(&mut store)), (ItemMemory { temporary_name: Some(under), ..ItemMemory::default() }, vec![address("update")]));
}

/// `give_up_binding` of a directory: what was asked of the directory of OneDrive is with the
/// row that has its binding now — the openings in it, and the empty files of the sessions
/// in it, which are abandoned — and what OneDrive answered about sending an item into it is
/// dropped, a taken name and a refusal: the directory the folder shows is one OneDrive does
/// not have yet. Of a file removed in it, the refusal of the removal
/// stands: it is removed from the same directory of OneDrive.
#[test]
fn a_directory_that_gives_its_binding_up_leaves_what_was_asked_of_it_with_the_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "docs");
    synced(&mut store, directory, "D");
    let (sending, asking, refused, removed) =
        (make_file(&mut store, directory, "sending"), make_file(&mut store, directory, "asking"), make_file(&mut store, directory, "straße"), make_file(&mut store, directory, "removed"));
    let (session, taken, refusal) = (new_file_session(&mut store, sending, "1"), taken_at(&mut store, refused, "STRASSE"), refusal_of(&mut store, refused, Reason::Forbidden));
    assert_eq!(remember_session(&mut store, sending, &session), Continued);
    store.step_memory().remember_upload_session_opening(asking, &place(directory, "asking"), 10).unwrap();
    assert_eq!(remember_taken(&mut store, refused, &taken), StillSaid);
    assert_eq!(remember_refused(&mut store, refused, &refusal), StillSaid);
    synced(&mut store, removed, "X");
    remove(&mut store, removed);
    let removal = removal_stamp(&mut store, removed);
    let refused_removal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal };
    assert_eq!(remember_refused(&mut store, removed, &refused_removal), StillSaid);

    let taker = store.item_store().give_up_binding(directory, false).unwrap();
    assert_eq!((windows(&mut store, &place(directory, "asking")), windows(&mut store, &place(taker, "asking"))), (vec![], vec![(10, 10)]));
    assert_eq!((said(&mut store, sending), abandoned(&mut store)), (ItemMemory::default(), vec![address("1")]));
    assert_eq!(store.step_memory().list_empty_file_sessions(&place(taker, "sending")).unwrap(), [EmptyFileSession::Abandoned { address: address("1") }]);
    assert_eq!((store.step_memory().list_taken_names(directory).unwrap(), kept(&store, refused)), (vec![], ItemMemory::default()), "the refusal goes with the taken name");
    assert_eq!(removal_stamp(&mut store, removed), removal, "the same removal, agreed to be in the directory the taker has now");
    assert_eq!(said(&mut store, removed), ItemMemory { unsent_since: Some(removal), refusal: Some(refused_removal), ..ItemMemory::default() });
}

/// `merge_items`: the local item takes the binding, and every fact of an unsent change of it
/// as a new item is dropped, the taken name among them, its session to the cancellations;
/// the item an upload left is left to the daemon to remove, as wherever its file gets a
/// binding.
/// The openings in a directory of OneDrive that is merged are in the local directory.
#[test]
fn items_that_are_merged_drop_the_new_items_session_and_keep_the_openings_of_the_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_file(&mut store, ROOT, "report");
    let before = remember_everything_of_a_shown_file(&mut store, local);
    let onedrive = store.item_store().make_onedrive_item(&reference("A"), ItemKind::File).unwrap();
    store.item_store().merge_items(local, onedrive).unwrap();
    assert_eq!(kept(&store, local), ItemMemory::default());
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), before.mismatched_item.into_iter().collect::<Vec<_>>());
    assert_eq!(abandoned(&mut store), [address("1")]);

    let local_directory = make_directory(&mut store, ROOT, "docs");
    let onedrive_directory = store.item_store().make_onedrive_item(&reference("D"), ItemKind::Directory).unwrap();
    store.step_memory().remember_upload_session_opening(local, &place(onedrive_directory, "x"), 10).unwrap();
    store.item_store().merge_items(local_directory, onedrive_directory).unwrap();
    assert_eq!(windows(&mut store, &place(local_directory, "x")), [(10, 10)]);
}

/// The root takes its binding from a row a listing made for it first: the openings in that
/// row are in the root, as with every binding that moves.
#[test]
fn the_openings_in_a_row_that_had_the_roots_binding_are_in_the_root() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let listed_first = store.item_store().make_onedrive_item(&reference("R"), ItemKind::Directory).unwrap();
    store.step_memory().remember_upload_session_opening(file, &place(listed_first, "report"), 10).unwrap();
    crate::item_store::bindings::bind_root(&store.conn, &reference("R")).unwrap();
    assert_eq!((store.item_store().find_item(listed_first).unwrap(), windows(&mut store, &place(ROOT, "report"))), (None, vec![(10, 10)]));
}

/// A directory is made here as `plans`, and is made in
/// OneDrive under a temporary name, remembered before the request; the answer is lost. A
/// listing brings the item under the temporary name, and the row is merged with the
/// directory. The temporary name is the directory's still: it says where OneDrive has the
/// item, which nothing agreed on says, and its step finishes the exchange. Merged with an
/// item OneDrive has under another name, or put over the binding of another file, the
/// temporary name was not that item's, and goes.
#[test]
fn a_new_item_merged_with_the_item_under_its_temporary_name_keeps_the_name() {
    use crate::item_store::tests::listed_drive::{listed_directory, store_with_listed_root, write_listing_of_changes};
    for (listed_name, is_kept) in [(".swap", true), ("Plans", false)] {
        let mut store = store_with_listed_root();
        let plans = make_directory(&mut store, ROOT, "plans");
        let under = TemporaryName { name: ".swap".into(), toward: place(ROOT, "plans") };
        store.step_memory().remember_temporary_name(plans, &under).unwrap();
        write_listing_of_changes(&mut store, &[listed_directory("N", "R", listed_name, "e-N")]);
        let listed = store.item_store().find_bound_item(&crate::item_store::tests::listed_drive::reference("N")).unwrap().unwrap().local_item_id;
        store.item_store().merge_items(plans, listed).unwrap();
        assert_eq!(kept(&store, plans).temporary_name, is_kept.then(|| under.clone()), "merged with the item OneDrive has as {listed_name}");
    }
    // Bound by the item's id alone, before any listing said where OneDrive has it.
    let mut store = store_with_listed_root();
    let plans = make_directory(&mut store, ROOT, "plans");
    let under = TemporaryName { name: ".swap".into(), toward: place(ROOT, "plans") };
    store.step_memory().remember_temporary_name(plans, &under).unwrap();
    store.item_store().bind_item(plans, &crate::item_store::tests::listed_drive::reference("N")).unwrap();
    assert_eq!(kept(&store, plans).temporary_name, Some(under));
    write_listing_of_changes(&mut store, &[listed_directory("N", "R", "plans", "e-N")]);
    assert_eq!(kept(&store, plans).temporary_name, None, "a listing says OneDrive has it under another name");
}

/// The item that holds a taken name gives its binding up — it was changed here and in
/// OneDrive, and is sent again as a new item — or its file is saved over: the binding, and
/// OneDrive's place with it, is another row's from then on. OneDrive answered nothing
/// about the name, which the item of that binding holds still: the taken name stays.
#[test]
fn a_holder_whose_binding_moves_to_another_row_leaves_the_name_taken() {
    use crate::item_store::tests::listed_drive::{listed_file, reference, store_with_listed_root, write_listing_of_changes};
    for is_saved_over in [false, true] {
        let mut store = store_with_listed_root();
        write_listing_of_changes(&mut store, &[listed_file("H", "R", "Holder", (3, "h-H"), "e-H")]);
        let holder = store.item_store().find_bound_item(&reference("H")).unwrap().unwrap().local_item_id;
        let planned_from = crate::sync_plan::ItemVersions::of(&item(&mut store, holder));
        assert_eq!(store.item_store().commit_show_item(&planned_from, false).unwrap(), crate::item_store::StepCommitOutcome::Written);
        let file = make_file(&mut store, ROOT, "report");
        assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(ROOT, "report"), holder_name: "Holder".into() }), StillSaid);
        if is_saved_over {
            let saved = make_file(&mut store, ROOT, "holder.tmp");
            store.item_store().apply(&LocalVersionChange::SavedOver { replacing: saved, replaced: holder }).unwrap();
        } else {
            change_content(&mut store, holder);
            store.item_store().give_up_binding(holder, false).unwrap();
        }
        let has_the_name = store.item_store().find_bound_item(&reference("H")).unwrap().unwrap();
        assert_ne!(has_the_name.local_item_id, holder, "another row has the binding");
        assert_eq!(store.step_memory().list_taken_names(ROOT).unwrap().into_iter().map(|taken| taken.item).collect::<Vec<_>>(), [file], "saved over: {is_saved_over}");
    }
}
