//! The commit of a removal that was sent: which row a file's is written to, and what the
//! removal of a directory asks of the rows below it.

use std::collections::BTreeSet;

use super::*;

/// The second review's S4: a file is made at the place a file was removed from, while that
/// removal is being sent, and takes its binding. What OneDrive confirmed — the item is
/// gone — is written to the row that has the binding now: the made file gives the binding
/// up to a row that is left with it alone, stamped, and is the user's own, sent as a new
/// item by the next plan. Nothing says OneDrive has what it confirmed it removed.
#[test]
fn the_removal_of_a_file_is_confirmed_of_the_row_that_has_the_binding_now() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let planned_from = versions(&mut store, file);
    let content = Some(ContentVersion::ChangedHere { size: 9, mtime: 400, mtime_nsec: 1, change_count: 1 });
    let made = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(ROOT, "g"), content: content.clone(), is_name_ignored: false, replaced: Some(file) }).unwrap();
    assert_eq!((store.item_store().find_item(file).unwrap(), bound(&mut store, "G").local_item_id), (None, made), "the made file has the binding");

    assert_eq!(store.item_store().commit_send_file_removal(&planned_from).unwrap(), StepCommitOutcome::Written);
    let removed = bound(&mut store, "G");
    assert!(removed.local_item_id != made && is_binding_alone(&removed) && removed.confirmed_at_count > 0, "{removed:?}");
    let made = row(&mut store, made);
    assert_eq!((made.onedrive_item.clone(), made.onedrive.clone(), made.synced_place.clone(), made.synced_content.clone()), (None, None, None, None), "the made file is an item OneDrive does not have");
    assert_eq!(made.local, Some(ItemVersion { place: Some(place(ROOT, "g")), content }));
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);

    // The same commit again finds OneDrive's side changed, whichever row has the binding.
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_file_removal(&planned_from)));
}

/// A directory `d` with the file `f` in it, both shown and then removed here, and the file
/// `g` shown in the root: the ids of the three, the directory's first.
fn store_of_removed_directory() -> (TreeStore, [LocalItemId; 3]) {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (3, "h-F"), "e-F")]);
    let (inside, kept) = (show(&mut store, "F"), show_file(&mut store, "G", "g"));
    for item in [inside, directory] {
        store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
    }
    (store, [directory, inside, kept])
}

/// Whether the removal can be sent, with nothing said by the caller.
fn can_be_sent(store: &mut TreeStore, item: &ItemVersions, below: &[ItemVersions]) -> bool {
    store.item_store().can_directory_removal_be_sent(item, below, &BTreeSet::new(), &BTreeSet::new()).unwrap()
}

/// The first review's S2 and the brief's §8: the store answers, to a step before its
/// request leaves, what the plan asked, of the rows as they are then; and the commit asks
/// it again. The removal of a directory can be sent while all the rows say OneDrive has
/// below it was removed here with it, as it was planned.
#[test]
fn the_removal_of_a_directory_can_be_sent_while_all_onedrive_has_below_it_was_removed_here() {
    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    assert!(can_be_sent(&mut store, &planned_from, &below));
    // Versions that are not the rows' any more are no removal to send: of the directory, of
    // what was removed with it, and a plan that did not name all that is below.
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..planned_from.clone() };
    assert!(!can_be_sent(&mut store, &outdated, &below));
    assert!(!can_be_sent(&mut store, &planned_from, &[ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..below[0].clone() }]));
    assert!(!can_be_sent(&mut store, &planned_from, &[]));
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &[])));
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
}

/// A listing moved an item the folder shows into the directory meanwhile: its removal
/// would take what is in the folder.
#[test]
fn the_removal_of_a_directory_is_not_sent_while_the_folder_shows_what_onedrive_has_below_it() {
    let (mut store, [directory, inside, kept]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    write_listing_of_changes(&mut store, &[listed_file("G", "D", "g", (3, "h-G"), "e-G2")]);
    assert!(!can_be_sent(&mut store, &planned_from, &below));
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &below)));
    assert!(row(&mut store, kept).onedrive.is_some());
}

/// The third review's S3, at the step and at the commit: a listing says OneDrive changed
/// the content of the file that was removed here with the directory. That file comes
/// back, so the removal is not sent, whatever versions it is asked with — those it was
/// planned from, or the rows' own — and a commit that comes all the same writes nothing.
#[test]
fn the_removal_of_a_directory_is_not_sent_once_onedrive_changed_the_content_of_a_file_removed_here_below_it() {
    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (5, "h-F2"), "e-F2")]);
    let below_now = [versions(&mut store, inside)];
    for below in [&below, &below_now] {
        assert!(!can_be_sent(&mut store, &planned_from, below));
        assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, below)));
    }
    assert!(bound(&mut store, "F").onedrive.is_some());
    // OneDrive renamed it and no more: the removal stands, and is sent with it.
    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f-renamed", (3, "h-F"), "e-F2")]);
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    assert!(can_be_sent(&mut store, &planned_from, &below));
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
}

/// The second review's M1 and M3, at the step: a listing brought a file OneDrive made in the
/// directory, which the folder never showed. The removal is not sent, and a commit that
/// comes all the same writes nothing: the rows do not say all of it was removed here.
#[test]
fn the_removal_of_a_directory_is_not_sent_while_onedrive_has_below_it_what_the_folder_never_showed() {
    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    write_listing_of_changes(&mut store, &[listed_file("N", "D", "new.txt", (4, "h-N"), "e-N")]);
    assert!(!can_be_sent(&mut store, &planned_from, &below));
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &below)));
    assert!(bound(&mut store, "N").onedrive.is_some());
}

/// The second review's M2 and S5, at the step: a request of this client that is not
/// settled sends an item into the directory or into one below it; the caller says so, and
/// the removal is not sent. Nor while a listing is under way. The commit asks the rows
/// alone: once the request went through there is nothing to hold back.
///
/// The third review's S6: the removal's own request is on record before it leaves, and
/// names the directory and what is removed with it. The caller gives the sets as it gives
/// them to the plan, that record in them, and the removal is not held back by itself.
#[test]
fn the_removal_of_a_directory_is_not_sent_while_a_request_not_settled_sends_into_it_or_a_listing_is_under_way() {
    let (mut store, [directory, inside, kept]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    let can_be_sent = |store: &mut TreeStore, running: &[LocalItemId], destinations: &[LocalItemId]| {
        store.item_store().can_directory_removal_be_sent(&planned_from, &below, &running.iter().copied().collect(), &destinations.iter().copied().collect()).unwrap()
    };
    assert!(can_be_sent(&mut store, &[], &[]));
    assert!(!can_be_sent(&mut store, &[], &[directory]), "an item is sent into it");
    assert!(can_be_sent(&mut store, &[directory], &[]), "the removal's own request names the directory");
    assert!(can_be_sent(&mut store, &[inside], &[]), "and what is removed with it");
    assert!(can_be_sent(&mut store, &[directory, inside, kept], &[kept]), "what is said of an item elsewhere holds nothing");
    assert!(!can_be_sent(&mut store, &[directory, inside], &[directory]), "its own request sends nothing into it: another one does");
    let listing = store.item_store().begin_delta_listing().unwrap();
    assert!(!can_be_sent(&mut store, &[], &[]), "a listing is under way");
    store.item_store().write_listed_changes(listing.id, &[], listing.began_at_count, &crate::ListingLink::DeltaLink("link".into())).unwrap();
    assert!(can_be_sent(&mut store, &[], &[]));
}

/// The second review's M3, its sequence: the removal of `d` is confirmed while a listing is
/// under way, and the listing's page, fetched before, names a file OneDrive made in `d` and
/// says `g` was moved into it. Neither is written: the rows keep the rule that an item's
/// directory exists in OneDrive, and the three are answered, to be read again.
#[test]
fn a_page_fetched_before_a_directory_removal_was_confirmed_puts_nothing_into_the_directory() {
    let (mut store, [directory, inside, kept]) = store_of_removed_directory();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    let listing = store.item_store().begin_delta_listing().unwrap();
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
    let page = [listed_file("N", "D", "new.txt", (4, "h-N"), "e-N"), listed_file("G", "D", "g", (3, "h-G"), "e-G2"), listed_file("F", "D", "f", (3, "h-F"), "e-F")];
    let outcome = store.item_store().write_listed_changes(listing.id, &page, listing.began_at_count, &crate::ListingLink::DeltaLink("link".into())).unwrap();
    assert_eq!(outcome.changes, [crate::ListedChangeOutcome::Outdated; 3]);
    let read_again: Vec<&str> = outcome.outdated_items.iter().map(|id| id.as_str()).collect();
    assert_eq!(read_again, ["N", "D", "G", "F"]);
    assert!(store.item_store().find_bound_item(&reference("N")).unwrap().is_none() && is_binding_alone(&bound(&mut store, "F")));
    assert_eq!(row(&mut store, kept).onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()), Some(place(ROOT, "g")));
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), true), None);
}

/// The removal the plan answers for `directory` from every row of the store, as the
/// daemon's caller describes them: the versions it was planned from, and what is removed
/// with it. `None` when the plan sends no removal of it.
fn plan_directory_removal(store: &mut TreeStore, directory: LocalItemId) -> Option<(ItemVersions, Vec<ItemVersions>)> {
    use crate::sync_plan::tests::convergence::plan_input::{describe_every_item, CallerFacts};
    let plan = crate::make_sync_plan(&describe_every_item(store, &CallerFacts::of_read_write_folder()));
    plan.steps.into_iter().find_map(|step| match step {
        crate::SyncStep::OneDrive(crate::OneDriveStep::SendDirectoryRemoval { item, below }) if item.local_item_id == directory => Some((item, below)),
        _ => None,
    })
}

/// The plan, the step's question and the commit agree on a directory's removal with what
/// an upload of this folder left in it: the item is the folder's own, not one it never
/// showed. It is not among what is removed here with the directory, holds the removal
/// back by nothing, and its row loses its OneDrive version with the directory's. Once the
/// item is not this folder's to remove any more — another changed it — it is an item the
/// folder never showed, to all three.
#[test]
fn the_removal_of_a_directory_is_planned_asked_and_committed_with_what_an_upload_left_in_it() {
    let store_with_what_an_upload_left = || {
        let (mut store, [directory, inside, _]) = store_of_removed_directory();
        let uploaded = make(&mut store, ItemKind::File, ROOT, "n");
        let standing = store.step_memory().remember_mismatched_item(uploaded, &crate::MismatchedItem::of_answer(reference("LEFT"), Some("c-h-LEFT"), None)).unwrap();
        assert_eq!(standing, crate::MismatchedItemStanding::LeftToRemove);
        write_listing_of_changes(&mut store, &[listed_file("LEFT", "D", "left.txt", (4, "h-LEFT"), "e-LEFT")]);
        (store, directory, inside)
    };
    let (mut store, directory, inside) = store_with_what_an_upload_left();
    let (planned_from, below) = plan_directory_removal(&mut store, directory).expect("the plan sends the removal");
    assert_eq!((&planned_from, below.as_slice()), (&versions(&mut store, directory), [versions(&mut store, inside)].as_slice()));
    assert!(can_be_sent(&mut store, &planned_from, &below));
    let with_what_was_left = [below[0].clone(), ItemVersions::of(&bound(&mut store, "LEFT"))];
    assert!(!can_be_sent(&mut store, &planned_from, &with_what_was_left), "what was left is not of what is removed here");
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
    for item_id in ["D", "F", "LEFT"] {
        assert!(is_binding_alone(&bound(&mut store, item_id)), "{item_id}");
    }
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);

    let (mut store, directory, inside) = store_with_what_an_upload_left();
    let (planned_from, below) = (versions(&mut store, directory), [versions(&mut store, inside)]);
    store.step_memory().forget_mismatched_item(&reference("LEFT")).unwrap();
    assert_eq!(plan_directory_removal(&mut store, directory), None);
    assert!(!can_be_sent(&mut store, &planned_from, &below));
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &below)));
    assert!(bound(&mut store, "LEFT").onedrive.is_some());
}

/// The review's M4, at the plan, the step and the commit: the content OneDrive has of the
/// file that was removed here with the directory is what an upload of this folder left in
/// place of what it sent. That is no change of OneDrive's, and the file is removed with
/// the directory. Once a listing writes another content the fact says nothing, and the
/// removal is not sent: the file comes back.
#[test]
fn the_removal_of_a_directory_is_planned_asked_and_committed_with_a_file_whose_content_is_the_own_upload() {
    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (5, "h-F2"), "e-F2")]);
    assert_eq!(plan_directory_removal(&mut store, directory), None, "not told: OneDrive changed the file");
    let left = row(&mut store, inside);
    let own_upload = crate::OwnUploadContent { onedrive_content: left.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone()).unwrap(), local_content: left.synced_content.clone().unwrap() };
    store.item_store().remember_own_upload_content(inside, &own_upload).unwrap();

    let (planned_from, below) = plan_directory_removal(&mut store, directory).expect("the plan sends the removal");
    assert_eq!((&planned_from, below.as_slice()), (&versions(&mut store, directory), [versions(&mut store, inside)].as_slice()));
    assert!(can_be_sent(&mut store, &planned_from, &below));

    // OneDrive changed the file after all: the fact is of another content.
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (6, "h-F3"), "e-F3")]);
    let below_now = [versions(&mut store, inside)];
    assert_eq!(plan_directory_removal(&mut store, directory), None);
    assert!(!can_be_sent(&mut store, &planned_from, &below_now));
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &below_now)));

    let (mut store, [directory, inside, _]) = store_of_removed_directory();
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (5, "h-F2"), "e-F2")]);
    let left = row(&mut store, inside);
    let own_upload = crate::OwnUploadContent { onedrive_content: left.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone()).unwrap(), local_content: left.synced_content.clone().unwrap() };
    store.item_store().remember_own_upload_content(inside, &own_upload).unwrap();
    let (planned_from, below) = plan_directory_removal(&mut store, directory).expect("the plan sends the removal");
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
    assert!(is_binding_alone(&bound(&mut store, "D")) && is_binding_alone(&bound(&mut store, "F")));
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);
}
