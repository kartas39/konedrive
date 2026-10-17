//! A listing of OneDrive as a test gives it to the store: the changes of a page in Graph's
//! words, and a store whose drive, `d1`, and root, `R`, a full listing told of.

use crate::model::{Change, DriveId, Kind, OneDriveItemId, OneDriveItemReference, Placement, Row};
use crate::{ListingLink, TreeStore};

/// The item `item_id` of the tests' drive.
pub(crate) fn reference(item_id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(item_id) }
}

/// The root of the drive as a listing gives it.
pub(crate) fn listed_root() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: Some("e-R".into()), ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

/// The directory `id` as a listing gives it: in `parent` under `name`, with the eTag `etag`.
pub(crate) fn listed_directory(id: &str, parent: &str, name: &str, etag: &str) -> Change {
    Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::Folder, size: 0, mtime: 500, etag: Some(etag.into()), ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

/// The file `id` as a listing gives it: in `parent` under `name`, with a content of `size`
/// bytes and the hash `hash`, whose cTag is `c-<hash>`, and the eTag `etag`.
pub(crate) fn listed_file(id: &str, parent: &str, name: &str, (size, hash): (u64, &str), etag: &str) -> Change {
    Change::Upsert(Row {
        id: id.into(),
        parent_id: Some(parent.into()),
        name: name.into(),
        kind: Kind::File,
        size,
        mtime: 1000,
        etag: Some(etag.into()),
        ctag: Some(format!("c-{hash}")),
        quickxor: Some(hash.into()),
        mime: None,
        placement: Placement::Placed,
    })
}

/// `change`, of an item the folder cannot show.
pub(crate) fn not_placed(change: Change) -> Change {
    match change {
        Change::Upsert(row) => Change::Upsert(Row { placement: Placement::Skipped(crate::model::SkipReason::Unsupported), ..row }),
        other => other,
    }
}

pub(crate) fn listed_gone(id: &str) -> Change {
    Change::Delete(id.into())
}

/// A store that knows its drive, whose root a full listing told of and ended.
pub(crate) fn store_with_listed_root() -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    let listing = store.item_store().begin_full_listing().unwrap();
    let outcome = store.item_store().write_listed_changes(listing.id, &[listed_root()], listing.began_at_count, &ListingLink::DeltaLink("link-0".into())).unwrap();
    assert!(outcome.is_link_written());
    store
}

/// A listing of changes that begins now, says `changes` in one page and ends.
pub(crate) fn write_listing_of_changes(store: &mut TreeStore, changes: &[Change]) {
    let listing = store.item_store().begin_delta_listing().unwrap();
    let outcome = store.item_store().write_listed_changes(listing.id, changes, listing.began_at_count, &ListingLink::DeltaLink("link".into())).unwrap();
    assert!(outcome.is_link_written(), "the listing left something to read again: {outcome:?}");
    assert!(outcome.changes.iter().all(|change| *change == crate::ListedChangeOutcome::Written), "the listing was not written whole: {outcome:?}");
}
