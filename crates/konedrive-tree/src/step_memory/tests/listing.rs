//! The facts of what OneDrive has against a listing: a listing is an answer of OneDrive, and
//! what it says of an item voids what a step remembered of that item, in the transaction
//! that writes the page.

use super::unsent_change::{show, store_with_docs};
use super::*;
use crate::item_store::tests::listed_drive::{listed_file, listed_gone, reference, write_listing_of_changes};
use crate::sync_plan::tests::convergence::plan_input::{plan_next_steps, CallerFacts, WideningCount};

/// The steps the plan made now has for `item`, each by its direction and its kind, when
/// the plan is given the names the store answers are taken in `directory` and that a
/// program holds `open` open for writing.
fn planned_steps(store: &mut TreeStore, directory: LocalItemId, open: &[LocalItemId], item: LocalItemId) -> Vec<String> {
    let mut facts = CallerFacts::of_read_write_folder();
    facts.places_taken_in_onedrive = store.step_memory().list_taken_names(directory).unwrap().into_iter().map(|taken| (taken.item, taken.taken_name.place)).collect();
    facts.items_open_for_writing = open.iter().copied().collect();
    let (_, plan) = plan_next_steps(store, &facts, &mut WideningCount::default());
    plan.steps.iter().filter(|step| step.local_item_id() == item).map(|step| format!("{step:?}").split(" {").next().unwrap_or_default().to_owned()).collect()
}

fn taken_names(store: &mut TreeStore, directory: LocalItemId) -> Vec<LocalItemId> {
    store.step_memory().list_taken_names(directory).unwrap().into_iter().map(|taken| taken.item).collect()
}

/// The third review's sequence. A new file is answered that its name is taken; a listing
/// brings the item that holds the name, while a program holds the file open for writing,
/// so that no plan decides it; a listing takes the holder away. The name is taken no more:
/// the file is sent under it, and is not renamed to a conflict copy.
#[test]
fn a_taken_name_is_void_once_a_listing_says_the_holder_is_gone() {
    let (mut store, docs) = store_with_docs();
    let file = make_file(&mut store, docs, "report");
    assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(docs, "report"), holder_name: "Report".into() }), StillSaid);
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (5, "h-N"), "e-N")]);
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    assert_eq!((taken_names(&mut store, docs), planned_steps(&mut store, docs, &[file], file)), (vec![file], vec![]), "the holder is listed: the name is taken, and the file is not planned");

    write_listing_of_changes(&mut store, &[listed_gone("N")]);
    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    assert_eq!(taken_names(&mut store, docs), [], "OneDrive answered again about the holder");
    assert_eq!(kept(&store, file).taken_name, None, "forgotten with the page, not only left unanswered");
    assert_eq!(planned_steps(&mut store, docs, &[], file), ["OneDrive(SendNewFile"]);
}

/// The holder is renamed in OneDrive, or moved out of the directory: the name is free.
#[test]
fn a_taken_name_is_void_once_a_listing_says_the_holder_left_the_name() {
    for (parent, name) in [("D", "Minutes"), ("R", "Report")] {
        let (mut store, docs) = store_with_docs();
        write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (5, "h-N"), "e-N")]);
        show(&mut store, "N");
        let file = make_file(&mut store, docs, "report");
        assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(docs, "report"), holder_name: "Report".into() }), StillSaid);
        assert_eq!(planned_steps(&mut store, docs, &[], file), ["Folder(RenameToConflictCopy"], "taken: the file steps aside");

        write_listing_of_changes(&mut store, &[listed_file("N", parent, name, (5, "h-N"), "e-N2")]);
        assert_eq!((taken_names(&mut store, docs), kept(&store, file).taken_name), (vec![], None), "the holder went to {parent}/{name}");
        assert_eq!(planned_steps(&mut store, docs, &[], file), ["OneDrive(SendNewFile"], "the file is sent under its name");
    }
}

/// What a listing says of the holder that leaves it under the name — another content,
/// another eTag — voids nothing: the name is taken as it was.
#[test]
fn a_taken_name_stays_while_the_listings_leave_the_holder_under_the_name() {
    let (mut store, docs) = store_with_docs();
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (5, "h-N"), "e-N")]);
    let file = make_file(&mut store, docs, "report");
    assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(docs, "report"), holder_name: "Report".into() }), StillSaid);
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (9, "h-N2"), "e-N2"), listed_file("O", "D", "other", (1, "h-O"), "e-O")]);
    write_listing_of_changes(&mut store, &[]);
    assert_eq!(taken_names(&mut store, docs), [file]);
}

/// Another item is listed into the name while a row still says the holder has it: OneDrive
/// has answered again about that name, and the plan decides from the rows.
#[test]
fn a_taken_name_is_void_once_a_listing_puts_another_item_under_the_name() {
    let (mut store, docs) = store_with_docs();
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "Report", (5, "h-N"), "e-N")]);
    let file = make_file(&mut store, docs, "report");
    assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(docs, "report"), holder_name: "Report".into() }), StillSaid);
    write_listing_of_changes(&mut store, &[listed_file("M", "D", "REPORT", (7, "h-M"), "e-M")]);
    assert_eq!((taken_names(&mut store, docs), kept(&store, file).taken_name), (vec![], None));
}

/// The holder was made and removed in OneDrive before any listing named it: no row ever
/// said it had the name. The listing that ends after the answer says OneDrive has no item
/// under the name, and the taken name is void by that.
#[test]
fn a_taken_name_is_void_once_a_listing_ends_with_no_item_under_the_name() {
    let (mut store, docs) = store_with_docs();
    let file = make_file(&mut store, docs, "report");
    assert_eq!(remember_taken(&mut store, file, &TakenName { place: place(docs, "report"), holder_name: "Report".into() }), StillSaid);
    assert_eq!(taken_names(&mut store, docs), [file], "until OneDrive answers again");
    write_listing_of_changes(&mut store, &[listed_file("O", "D", "other", (1, "h-O"), "e-O")]);
    assert_eq!((taken_names(&mut store, docs), kept(&store, file).taken_name), (vec![], None));
    assert_eq!(planned_steps(&mut store, docs, &[], file), ["OneDrive(SendNewFile"]);
}

fn left(item_id: &str) -> MismatchedItem {
    MismatchedItem::of_answer(reference(item_id), Some(&format!("c-h-{item_id}")), None)
}

/// The item an upload left is an item of OneDrive, and a listing names it: with the tag
/// the upload's answer gave it is what the upload left still; with another tag, another
/// client wrote it since, and it is nobody's to remove; listed gone, nothing is left to
/// remove. Kept with its file or among the removals left, it is forgotten the same way.
#[test]
fn the_item_an_upload_left_is_void_once_a_listing_says_it_changed_or_is_gone() {
    for is_file_gone in [false, true] {
        let (mut store, docs) = store_with_docs();
        let (file, other) = (make_file(&mut store, docs, "report"), make_file(&mut store, docs, "other"));
        assert_eq!(store.step_memory().remember_mismatched_item(file, &left("BAD")).unwrap(), LeftToRemove);
        assert_eq!(store.step_memory().remember_mismatched_item(other, &left("WORSE")).unwrap(), LeftToRemove);
        if is_file_gone {
            remove(&mut store, file);
            remove(&mut store, other);
        }
        let found = |store: &mut TreeStore, item_id: &str| store.step_memory().find_mismatched_item(&reference(item_id)).unwrap().map(|upload| upload.mismatched_item);

        write_listing_of_changes(&mut store, &[listed_file("BAD", "D", "report", (0, "h-BAD"), "e-1"), listed_file("WORSE", "D", "other", (0, "h-WORSE"), "e-1")]);
        assert_eq!((found(&mut store, "BAD"), found(&mut store, "WORSE")), (Some(left("BAD")), Some(left("WORSE"))), "listed as the uploads left them: still to remove");

        write_listing_of_changes(&mut store, &[listed_file("BAD", "D", "report", (4, "h-theirs"), "e-2"), listed_gone("WORSE")]);
        assert_eq!((found(&mut store, "BAD"), found(&mut store, "WORSE")), (None, None), "file gone: {is_file_gone}");
        assert_eq!((store.step_memory().list_mismatched_item_removals(10).unwrap(), count(&store, "mismatched_item_removals")), (vec![], 0));
        if !is_file_gone {
            assert_eq!((kept(&store, file).mismatched_item, kept(&store, other).mismatched_item), (None, None));
        }
    }
}
