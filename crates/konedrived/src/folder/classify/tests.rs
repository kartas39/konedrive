use serde_json::json;

use super::*;

fn item(value: serde_json::Value) -> DriveItem {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_file_item_becomes_a_placed_file_row() {
    let change = classify(&item(json!({
        "id": "F", "name": "a.jpg", "size": 7, "eTag": "e", "cTag": "c",
        "parentReference": {"id": "R"},
        "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
    })));
    let Change::Upsert(row) = change else { panic!("{change:?}") };
    assert_eq!((row.kind, row.size, row.mtime, row.placement), (Kind::File, 7, 1_714_557_600, Placement::Placed));
    assert_eq!((row.ctag.as_deref(), row.quickxor.as_deref(), row.mime.as_deref()), (Some("c"), Some("q"), Some("image/jpeg")));
    assert_eq!(row.parent_id.as_deref(), Some("R"));
}

#[test]
fn the_root_deletions_and_folders_are_told_apart() {
    assert!(matches!(classify(&item(json!({"id": "R", "root": {}, "folder": {}}))), Change::Root(_)));
    assert_eq!(classify(&item(json!({"id": "X", "deleted": {"state": "deleted"}}))), Change::Delete("X".into()));
    let Change::Upsert(row) = classify(&item(json!({"id": "D", "name": "d", "size": 999, "folder": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!((row.kind, row.size), (Kind::Folder, 0), "a folder's size is its content's, not a file size");
}

#[test]
fn every_skip_reason_is_recognised() {
    let cases = [
        (json!({"id": "1", "name": "я".repeat(128), "file": {}, "parentReference": {"id": "R"}}), SkipReason::NameTooLong),
        (json!({"id": "2", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}), SkipReason::PersonalVault),
        (json!({"id": "3", "name": "shared", "folder": {}, "remoteItem": {"id": "x"}, "parentReference": {"id": "R"}}), SkipReason::Shared),
        (json!({"id": "4", "name": "Notes", "package": {"type": "oneNote"}, "parentReference": {"id": "R"}}), SkipReason::OneNote),
        (json!({"id": "5", "name": ".konedrive-holding", "folder": {}, "parentReference": {"id": "R"}}), SkipReason::ReservedName),
        (json!({"id": "6", "name": "odd", "parentReference": {"id": "R"}}), SkipReason::Unsupported),
        (json!({"id": "7", "name": "..", "file": {}, "parentReference": {"id": "R"}}), SkipReason::Unsupported),
    ];
    for (value, reason) in cases {
        let Change::Upsert(row) = classify(&item(value.clone())) else { panic!("{value}") };
        assert_eq!(row.placement, Placement::Skipped(reason), "{value}");
    }
    let Change::Upsert(fits) = classify(&item(json!({"id": "8", "name": "я".repeat(127), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fits.placement, Placement::Placed, "127 Cyrillic letters are 254 bytes and fit");
}

#[test]
fn the_255_byte_name_is_the_exact_boundary() {
    let Change::Upsert(exact) = classify(&item(json!({"id": "9", "name": "a".repeat(255), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(exact.placement, Placement::Placed, "255 bytes is Linux's limit, inclusive");
    let Change::Upsert(over) = classify(&item(json!({"id": "10", "name": "a".repeat(256), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(over.placement, Placement::Skipped(SkipReason::NameTooLong), "256 bytes is one over");
}

/// An item id becomes a name in the holding directory: an id that cannot
/// be one keeps the item out of the folder.
#[test]
fn an_id_that_cannot_be_a_file_name_is_not_placed() {
    for id in ["", ".", "..", "a/b", "a\0b"] {
        let Change::Upsert(row) = classify(&item(json!({"id": id, "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!("{id:?}") };
        assert_eq!(row.placement, Placement::Skipped(SkipReason::Unsupported), "{id:?}");
    }
    let Change::Upsert(fine) = classify(&item(json!({"id": "8F6C!101", "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fine.placement, Placement::Placed);
}

mod listed_into_items {
    //! The changes `classify` makes of the pages the drive client fetches, given to the
    //! entry that writes a listing into the rows of `items`
    //! (`ItemStore::write_listed_changes`): the rows then say what the fake OneDrive has.
    //! The pages are asked for as a cycle asks for them, one after the other until the one
    //! with the delta link; nothing of today's staging is in between.

    use konedrive_graph::drive::{DeltaFrom, DeltaNext};
    use konedrive_tree::{Change, ContentVersion, DriveId, ItemKind, OneDriveItemId, OneDriveItemReference, TreeStore};

    use super::classify;
    use crate::fake_onedrive::{FakeGraph, FakeItem, ROOT};

    fn reference(id: &str) -> OneDriveItemReference {
        OneDriveItemReference { drive_id: DriveId::new("D").unwrap(), item_id: OneDriveItemId::new(id) }
    }

    fn folder(id: &str, parent: &str, name: &str) -> FakeItem {
        FakeItem { id: id.into(), parent: Some(parent.into()), name: name.into(), folder: true, content: Vec::new(), hash: None, size: 0, etag: format!("e-{id}"), ctag: format!("c-{id}"), mtime: 0 }
    }

    /// Lists the fake's drive from `from` to the delta link, each page written as it comes.
    async fn list(graph: &FakeGraph, store: &mut TreeStore, mut from: DeltaFrom) -> String {
        let drive = graph.client();
        let fetch_began_at_count = store.item_store().confirmation_count().unwrap();
        loop {
            let page = drive.delta(&from).await.unwrap();
            let changes: Vec<Change> = page.items.iter().map(classify).collect();
            match page.next {
                DeltaNext::Page(next) => {
                    assert_eq!(store.item_store().write_listed_changes(&changes, fetch_began_at_count, None).unwrap(), Vec::new());
                    from = DeltaFrom::Link(next);
                }
                DeltaNext::Done(link) => {
                    assert_eq!(store.item_store().write_listed_changes(&changes, fetch_began_at_count, Some(&link)).unwrap(), Vec::new());
                    return link;
                }
            }
        }
    }

    /// Each of `ids` has in its row what the fake has of it: its kind, its directory, its
    /// name, for a file its content, and its eTag. And the rows hold nothing of OneDrive's
    /// beside them.
    fn assert_rows_say_what_the_fake_has(graph: &FakeGraph, store: &mut TreeStore, ids: &[&str]) {
        for id in ids {
            let fake = graph.with(|cloud| cloud.item(id).cloned()).unwrap_or_else(|| panic!("the fake has no item {id}"));
            let row = store.item_store().find_bound_item(&reference(id)).unwrap().unwrap_or_else(|| panic!("the item {id} has no row"));
            let onedrive = row.onedrive.clone().unwrap_or_else(|| panic!("the row of {id} has no OneDrive version"));
            let place = onedrive.place.unwrap();
            let directory = store.item_store().find_item(place.parent_local_item_id).unwrap().unwrap();
            assert_eq!(directory.onedrive_item, Some(reference(fake.parent.as_deref().unwrap())), "the directory of {id}");
            assert_eq!(place.name.to_str(), Some(fake.name.as_str()), "the name of {id}");
            assert_eq!(row.kind, if fake.folder { ItemKind::Directory } else { ItemKind::File }, "the kind of {id}");
            let content = (!fake.folder).then(|| ContentVersion::Hashed { size: fake.size, mtime: fake.mtime, mtime_nsec: 0, hash: fake.hash.clone(), ctag: Some(fake.ctag.clone()) });
            assert_eq!(onedrive.content, content, "the content of {id}");
            assert_eq!(row.onedrive_etag.as_deref(), Some(fake.etag.as_str()), "the eTag of {id}");
            assert!(store.item_store().can_be_shown(row.local_item_id).unwrap(), "{id} can be shown");
        }
        // Nothing is agreed on yet, so every item OneDrive has is one that it changed.
        let listed = store.item_store().list_changed_in_onedrive(None, 1000).unwrap();
        let mut listed: Vec<String> = listed.into_iter().filter(|item| item.onedrive.is_some()).map(|item| item.onedrive_item.unwrap().item_id.as_str().to_owned()).collect();
        listed.sort();
        let mut expected: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
        expected.sort();
        assert_eq!(listed, expected);
    }

    /// A drive listed in full, and then its changes: a file moved and renamed, another
    /// edited, a folder deleted with what is in it, a file and a folder made.
    #[test]
    fn the_rows_of_items_say_what_the_fake_onedrive_has_after_a_full_listing_and_after_its_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let graph = FakeGraph::start().await;
            graph.with(|cloud| {
                cloud.add(folder("DOCS", ROOT, "Docs"));
                cloud.add(folder("OLD", "DOCS", "Old"));
                cloud.add_file("A", "DOCS", "a.txt", b"alpha");
                cloud.add_file("B", ROOT, "b.txt", b"beta");
                cloud.add_file("C", "OLD", "c.txt", b"gamma");
            });
            let mut store = TreeStore::in_memory().unwrap();
            store.set_drive_id("D").unwrap();

            store.item_store().begin_full_listing().unwrap();
            let link = list(&graph, &mut store, DeltaFrom::Start).await;
            assert_eq!(store.delta_link().unwrap().as_deref(), Some(link.as_str()));
            let root = store.item_store().find_bound_item(&reference(ROOT)).unwrap().unwrap();
            assert_eq!(root.local_item_id, konedrive_tree::LocalItemId::ROOT);
            assert_rows_say_what_the_fake_has(&graph, &mut store, &["DOCS", "OLD", "A", "B", "C"]);

            graph.with(|cloud| {
                cloud.rename("A", ROOT, "moved.txt");
                cloud.edit("B", b"another content");
                cloud.trash("OLD");
                cloud.add(folder("NEW", ROOT, "New"));
                cloud.add_file("N", "NEW", "n.txt", b"new");
            });
            let next_link = list(&graph, &mut store, DeltaFrom::Link(link.clone())).await;
            assert_ne!(next_link, link);
            assert_rows_say_what_the_fake_has(&graph, &mut store, &["DOCS", "A", "B", "NEW", "N"]);
            for gone in ["OLD", "C"] {
                assert_eq!(store.item_store().find_bound_item(&reference(gone)).unwrap(), None, "{gone} is gone from OneDrive, and nothing here kept its row");
            }
        });
    }
}
