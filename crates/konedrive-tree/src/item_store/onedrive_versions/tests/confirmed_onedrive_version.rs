//! What OneDrive confirmed of an item to a step, written into its row, and the count of
//! confirmations.

use super::*;

/// A confirmation writes, in one transaction, the OneDrive version as Graph answered it and
/// that answer's eTag, raises the count of confirmations and stamps the row with it. Nothing
/// else of the row is written, but the MIME type, which is taken away: what the two sides
/// agree on is the step's to write.
#[test]
fn a_confirmation_writes_the_version_and_the_etag_and_stamps_the_row_with_the_next_count() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1"), directory("D", "R", "D")]);
    let (a, b, d) = (listed(&mut store, "A").local_item_id, listed(&mut store, "B").local_item_id, listed(&mut store, "D").local_item_id);
    let before = item(&mut store, a);
    assert_eq!(count(&mut store), 0);

    assert_eq!(store.item_store().confirm_onedrive_version(a, &present(d, "moved", Some("22"), "e-A-22")).unwrap(), 1);
    assert_eq!(item(&mut store, a), Item { onedrive: version(d, "moved", Some("22")), onedrive_etag: Some("e-A-22".to_owned()), onedrive_mime: None, confirmed_at_count: 1, ..before });
    assert_eq!(store.item_store().confirm_onedrive_version(b, &present(ROOT, "b", Some("1"), "e-B-1")).unwrap(), 2);
    assert_eq!(store.item_store().confirm_onedrive_version(a, &present(ROOT, "a", Some("333"), "e-A-333")).unwrap(), 3);
    assert_eq!((item(&mut store, a).confirmed_at_count, item(&mut store, b).confirmed_at_count, count(&mut store)), (3, 2, 3));
    assert_eq!(store.item_store().confirm_onedrive_version(d, &present(ROOT, "Renamed", None, "e-D-2")).unwrap(), 4);
    assert_eq!(listed(&mut store, "D").onedrive, version(ROOT, "Renamed", None));
}

/// A confirmed removal leaves the row with its binding alone, stamped: no version on either
/// side, no eTag, nothing agreed on. The row stays, so that a page fetched before the
/// removal finds the binding taken.
#[test]
fn a_confirmed_removal_leaves_the_row_with_its_binding_alone_stamped() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write(&mut store, &[file("K", "R", "kept", "1")]);
    store.item_store().apply(&LocalVersionChange::Removed { item: kept }).unwrap();
    assert!(item(&mut store, kept).synced_place.is_some());

    assert_eq!(store.item_store().confirm_onedrive_version(kept, &ConfirmedOneDriveVersion::Removed).unwrap(), 1);
    let tombstone = item(&mut store, kept);
    assert_eq!(
        (tombstone.onedrive_item.clone(), tombstone.local.clone(), tombstone.onedrive.clone(), tombstone.onedrive_etag.clone(), tombstone.synced_place.clone(), tombstone.synced_content.clone(), tombstone.confirmed_at_count),
        (Some(reference("K")), None, None, None, None, None, 1)
    );
    assert_eq!(bound(&mut store, "K"), Some(tombstone));
}

/// What OneDrive cannot have confirmed is refused, and is no confirmation: of an id with no
/// row, of an item with no binding, of the root as removed or with a place, a removal of an
/// item the folder still shows, a file with no content or a directory with one, a content
/// told by a change count, and a place in a file, in itself or in a directory with no row.
#[test]
fn a_confirmation_that_cannot_be_is_refused_and_raises_no_count() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), directory("D", "R", "D")]);
    let (a, d) = (listed(&mut store, "A").local_item_id, listed(&mut store, "D").local_item_id);
    let here = make_file(&mut store, ROOT, "made-here");
    let shown = make_file(&mut store, ROOT, "shown");
    synced(&mut store, shown, "S");
    let changed_here = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(place(ROOT, "a")), content: Some(ContentVersion::ChangedHere { size: 1, mtime: 1, mtime_nsec: 0, change_count: 1 }) }, etag: "e".to_owned() };
    let rows = every_row(&mut store);
    for (what, item_id, confirmed) in [
        ("an id with no row", LocalItemId::new(999), present(ROOT, "x", Some("1"), "e")),
        ("an item with no binding", here, present(ROOT, "made-here", Some("1"), "e")),
        ("the root removed", ROOT, ConfirmedOneDriveVersion::Removed),
        ("the root with a place", ROOT, present(d, "root", None, "e")),
        ("a removal of an item the folder shows", shown, ConfirmedOneDriveVersion::Removed),
        ("a file with no content", a, present(ROOT, "a", None, "e")),
        ("a directory with a content", d, present(ROOT, "D", Some("1"), "e")),
        ("a content told by a change count", a, changed_here),
        ("a place in a file", d, present(a, "D", None, "e")),
        ("a place in itself", d, present(d, "D", None, "e")),
        ("a place in a directory with no row", a, present(LocalItemId::new(999), "a", Some("1"), "e")),
    ] {
        assert!(is_refused(store.item_store().confirm_onedrive_version(item_id, &confirmed)), "{what}");
    }
    assert_eq!(count(&mut store), 0, "a refused confirmation is none");
    assert_eq!(every_row(&mut store), rows);
}

/// N8 of the second review. A confirmation says nothing of the MIME type OneDrive gives
/// for the content, which follows the name and the content: the one the row had is taken
/// away, and the next listing writes the one OneDrive has.
#[test]
fn a_confirmation_takes_away_the_mime_type_the_row_had() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a.txt", "1")]);
    let a = listed(&mut store, "A").local_item_id;
    assert_eq!(item(&mut store, a).onedrive_mime.as_deref(), Some("text/plain"));

    store.item_store().confirm_onedrive_version(a, &present(ROOT, "a.jpg", Some("22"), "e-A-22")).unwrap();
    assert_eq!(item(&mut store, a).onedrive_mime, None);
    write(&mut store, &[file("A", "R", "a.jpg", "22")]);
    assert_eq!(item(&mut store, a).onedrive_mime.as_deref(), Some("text/plain"), "as the listing gives it");
}
