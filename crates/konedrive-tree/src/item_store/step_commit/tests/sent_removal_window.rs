//! The removals this client sent since their window began
//! ([`SentRemovalWindow`](crate::SentRemovalWindow)): counted by the commits that wrote
//! them, and kept whatever became of the rows.

use super::*;
use crate::SentRemovalWindow;

fn window(store: &mut TreeStore) -> SentRemovalWindow {
    store.item_store().find_sent_removal_window().unwrap()
}

fn remove(store: &mut TreeStore, item: LocalItemId) -> ItemVersions {
    store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
    versions(store, item)
}

/// A listing of changes that says nothing begins and ends.
fn end_a_listing(store: &mut TreeStore) {
    write_listing_of_changes(store, &[]);
}

/// The second review's S2 (the first's S6): a removed file is one, a removed directory one
/// with each item that was removed with it; a commit that writes nothing counts nothing;
/// and a listing that ends in between, which takes the rows of the confirmed removals,
/// takes nothing from the count.
#[test]
fn the_removals_sent_are_counted_by_their_commits_whatever_listing_ended_since() {
    let mut store = store_with_listed_root();
    let files: Vec<LocalItemId> = ["A", "B", "C"].iter().map(|id| show_file(&mut store, id, &format!("{id}.txt"))).collect();
    let directory = show_directory(&mut store, "D", "R", "d");
    write_listing_of_changes(&mut store, &[listed_file("F", "D", "f", (3, "h-F"), "e-F")]);
    let inside = show(&mut store, "F");
    assert_eq!(window(&mut store), SentRemovalWindow { began_at_count: 0, removal_count: 0 });
    for file in &files[..2] {
        let planned_from = remove(&mut store, *file);
        assert_eq!(store.item_store().commit_send_file_removal(&planned_from).unwrap(), StepCommitOutcome::Written);
    }
    assert_eq!(window(&mut store).removal_count, 2);
    end_a_listing(&mut store);
    assert!(store.item_store().find_item(files[0]).unwrap().is_none(), "the row of a confirmed removal goes with the listing");
    assert_eq!(window(&mut store).removal_count, 2, "and is counted still");

    let (below, planned_from) = ([remove(&mut store, inside)], remove(&mut store, directory));
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..planned_from.clone() };
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&outdated, &below)));
    assert_eq!(window(&mut store).removal_count, 2, "a commit that writes nothing counts nothing");
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below).unwrap(), StepCommitOutcome::Written);
    assert_eq!(window(&mut store), SentRemovalWindow { began_at_count: 0, removal_count: 4 }, "the directory and what was removed with it");
}

/// The window begun anew counts from nothing, from the count of confirmations as it is.
#[test]
fn a_window_begun_anew_counts_from_nothing() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    let planned_from = remove(&mut store, file);
    assert_eq!(store.item_store().commit_send_file_removal(&planned_from).unwrap(), StepCommitOutcome::Written);
    assert_eq!(store.item_store().begin_sent_removal_window().unwrap(), SentRemovalWindow { began_at_count: 1, removal_count: 0 });
    assert_eq!(window(&mut store), SentRemovalWindow { began_at_count: 1, removal_count: 0 });
    let other = show_file(&mut store, "B", "b.txt");
    let planned_from = remove(&mut store, other);
    assert_eq!(store.item_store().commit_send_file_removal(&planned_from).unwrap(), StepCommitOutcome::Written);
    assert_eq!(window(&mut store), SentRemovalWindow { began_at_count: 1, removal_count: 1 });
}

/// The second review's S3: what another client removed is no removal this client sent,
/// also when a step of this client stamped the row before. This client sends new content
/// into two files; the other client removes them, and they go here.
#[test]
fn what_the_other_client_removed_is_not_counted() {
    let mut store = store_with_listed_root();
    for id in ["A", "B"] {
        let file = show_file(&mut store, id, &format!("{id}.txt"));
        write_here(&mut store, file, 7);
        let planned_from = versions(&mut store, file);
        let sent = confirmed(id, "R", &format!("{id}.txt"), Some(confirmed_content(7, "h-sent")), "e-sent");
        assert_eq!(store.item_store().commit_send_content(&planned_from, &sent, None).unwrap(), StepCommitOutcome::Written);
        let listing = store.item_store().begin_delta_listing().unwrap();
        let count = store.item_store().confirmation_count().unwrap();
        store.item_store().write_listed_changes(listing.id, &[listed_gone(id)], count, &crate::ListingLink::NextLink("next".into())).unwrap();
        let planned_from = versions(&mut store, file);
        assert_eq!(store.item_store().commit_remove_item(&planned_from, None).unwrap(), StepCommitOutcome::Written);
        assert!(is_binding_alone(&row(&mut store, file)) && row(&mut store, file).confirmed_at_count > 0);
    }
    assert_eq!(window(&mut store).removal_count, 0);
}
