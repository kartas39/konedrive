//! A listing told of an item before the answer to this client's own request about it came:
//! the commit writes what was agreed on, and does not put OneDrive's version back to what
//! the answer said where the listing said a later one.

use super::*;

/// Found by the convergence run in which OneDrive is a state of its own: a new file is
/// sent, another client writes into it at once, and the listing that says so is written
/// before the answer to the upload comes. The commit binds the file and agrees on what was
/// sent; what OneDrive has is what the listing said, under its eTag, so that the plan made
/// next takes OneDrive's content. Written back to the answer's, the rows would say an older
/// version for good: no listing names the item again.
#[test]
fn a_new_file_another_client_changed_before_the_answer_came_keeps_what_the_listing_said() {
    let mut store = store_with_listed_root();
    let file = make(&mut store, ItemKind::File, ROOT, "new.txt");
    let planned_from = versions(&mut store, file);
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "new.txt", (9, "h-theirs"), "e-theirs")]);
    let made = confirmed("N", "R", "new.txt", Some(confirmed_content(5, "h-sent")), "e-sent");
    assert_eq!(store.item_store().commit_send_new_file(&planned_from, &reference("R"), "new.txt".as_ref(), &made, None).unwrap(), StepCommitOutcome::Written);
    let now = row(&mut store, file);
    assert_eq!(now.onedrive_item, Some(reference("N")));
    assert_eq!((now.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone()), now.onedrive_etag.as_deref()), (Some(listed_content(9, "h-theirs")), Some("e-theirs")), "what the listing said OneDrive has");
    assert_eq!((now.synced_place.clone(), now.synced_content.clone()), (Some(place(ROOT, "new.txt")), Some(confirmed_content(5, "h-sent"))), "what was sent is agreed on");
    assert_eq!(now.local.as_ref().and_then(|local| local.content.clone()), Some(confirmed_content(5, "h-sent")));
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);

    // The listing said what the answer says: one version, written as confirmed.
    let other = make(&mut store, ItemKind::File, ROOT, "other.txt");
    let planned_from = versions(&mut store, other);
    write_listing_of_changes(&mut store, &[listed_file("O", "R", "other.txt", (5, "h-sent"), "e-sent")]);
    let made = confirmed("O", "R", "other.txt", Some(confirmed_content(5, "h-sent")), "e-sent");
    assert_eq!(store.item_store().commit_send_new_file(&planned_from, &reference("R"), "other.txt".as_ref(), &made, None).unwrap(), StepCommitOutcome::Written);
    assert!(agrees(&row(&mut store, other)), "{:?}", row(&mut store, other));
}
