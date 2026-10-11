//! The removals this client sent a short time ago
//! ([`SentRemovalWindow`](crate::SentRemovalWindow)): counted by the commits that wrote
//! them, kept whatever became of the rows, and over by themselves once the window's time
//! has passed since the last of them.

use super::*;
use crate::{SentRemovalWindow, SENT_REMOVAL_WINDOW_SECONDS};

/// The time of the first commit of each test, in Unix seconds.
const NOW: i64 = 1_700_000_000;

/// The window as of `NOW`.
fn window(store: &mut TreeStore) -> SentRemovalWindow {
    store.item_store().find_sent_removal_window(NOW).unwrap()
}

/// Shows a file, removes it here and commits its removal as confirmed at the time `at`.
fn send_removal(store: &mut TreeStore, id: &str, at: i64) {
    let file = show_file(store, id, &format!("{id}.txt"));
    let planned_from = remove(store, file);
    assert_eq!(store.item_store().commit_send_file_removal(&planned_from, at).unwrap(), StepCommitOutcome::Written);
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
    assert_eq!(window(&mut store), SentRemovalWindow::EMPTY);
    for file in &files[..2] {
        let planned_from = remove(&mut store, *file);
        assert_eq!(store.item_store().commit_send_file_removal(&planned_from, NOW).unwrap(), StepCommitOutcome::Written);
    }
    assert_eq!(window(&mut store).removal_count, 2);
    end_a_listing(&mut store);
    assert!(store.item_store().find_item(files[0]).unwrap().is_none(), "the row of a confirmed removal goes with the listing");
    assert_eq!(window(&mut store).removal_count, 2, "and is counted still");

    let (below, planned_from) = ([remove(&mut store, inside)], remove(&mut store, directory));
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..planned_from.clone() };
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&outdated, &below, NOW)));
    assert_eq!(window(&mut store).removal_count, 2, "a commit that writes nothing counts nothing");
    assert_eq!(store.item_store().commit_send_directory_removal(&planned_from, &below, NOW).unwrap(), StepCommitOutcome::Written);
    assert_eq!(window(&mut store), SentRemovalWindow { last_removal_at: Some(NOW), removal_count: 4 }, "the directory and what was removed with it");
}

/// The third review's S7: nobody ever began a window in this store, and the window ends
/// all the same. Nine removals within a short time are counted together; a single removal
/// a day later is one, at its commit and to who reads before it.
#[test]
fn a_window_nobody_began_is_over_once_its_time_passed_since_the_last_removal() {
    let mut store = store_with_listed_root();
    for number in 0..9 {
        send_removal(&mut store, &format!("A{number}"), NOW + number);
    }
    let last = NOW + 8;
    let read = |store: &mut TreeStore, now: i64| store.item_store().find_sent_removal_window(now).unwrap();
    assert_eq!(read(&mut store, last), SentRemovalWindow { last_removal_at: Some(last), removal_count: 9 });
    assert_eq!(read(&mut store, last + SENT_REMOVAL_WINDOW_SECONDS - 1).removal_count, 9, "within the window of the last removal");
    assert_eq!(read(&mut store, last + SENT_REMOVAL_WINDOW_SECONDS), SentRemovalWindow::EMPTY, "the window is over, though no removal was sent since and nobody began one");
    let a_day_later = last + 86_400;
    assert_eq!(read(&mut store, a_day_later), SentRemovalWindow::EMPTY);
    send_removal(&mut store, "B", a_day_later);
    assert_eq!(read(&mut store, a_day_later), SentRemovalWindow { last_removal_at: Some(a_day_later), removal_count: 1 }, "the nine of the day before are not counted with it");
}

/// The time counted from is that of the last removal: removals that follow one another
/// within the window are one window, however long they take together.
#[test]
fn removals_that_follow_one_another_within_the_window_are_counted_together() {
    let mut store = store_with_listed_root();
    let step = SENT_REMOVAL_WINDOW_SECONDS - 1;
    for number in 0..4 {
        send_removal(&mut store, &format!("A{number}"), NOW + number * step);
    }
    assert_eq!(store.item_store().find_sent_removal_window(NOW + 3 * step).unwrap().removal_count, 4);
}

/// The user's confirmation of a mass removal empties the window: what is sent after it is
/// counted from nothing.
#[test]
fn a_window_begun_anew_counts_from_nothing() {
    let mut store = store_with_listed_root();
    send_removal(&mut store, "A", NOW);
    assert_eq!(store.item_store().begin_sent_removal_window().unwrap(), SentRemovalWindow::EMPTY);
    assert_eq!(window(&mut store), SentRemovalWindow::EMPTY);
    send_removal(&mut store, "B", NOW);
    assert_eq!(window(&mut store), SentRemovalWindow { last_removal_at: Some(NOW), removal_count: 1 });
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
