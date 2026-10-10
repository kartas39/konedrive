use super::*;
use crate::{Counts, TreeStore};

/// The words the store keeps, spelled out: a store written by another
/// build reads the same, so a constant reworded fails here.
#[test]
fn the_stored_words_are_spelled_as_they_always_were() {
    assert_eq!(Kind::File.as_str(), "file");
    assert_eq!(Kind::Folder.as_str(), "folder");
    assert_eq!(Placement::Placed.encode(), "placed");
    let skips = [
        (SkipReason::NameTooLong, "skipped:name-too-long"),
        (SkipReason::PersonalVault, "skipped:personal-vault"),
        (SkipReason::Shared, "skipped:shared"),
        (SkipReason::OneNote, "skipped:onenote"),
        (SkipReason::ReservedName, "skipped:reserved-name"),
        (SkipReason::Unsupported, "skipped:unsupported"),
    ];
    for (reason, stored) in skips {
        assert_eq!(Placement::Skipped(reason).encode(), stored);
        assert_eq!(Placement::decode(stored), Placement::Skipped(reason));
    }
    assert_eq!(Placement::decode("placed"), Placement::Placed);
    assert_eq!(Kind::decode("folder"), Kind::Folder);
    assert_eq!(Kind::decode("file"), Kind::File);
}

/// A word nobody can read (a damaged store) never takes an item out of the
/// folder: a placement that is no placement is placed, for the decoder and
/// for every query alike, and the indexes of placed and skipped rows serve
/// those queries. A
/// skip nobody knows is skipped, as unsupported; a kind that is no kind is a
/// file.
#[test]
fn a_placement_nobody_can_read_is_placed_for_the_decoder_and_the_queries() {
    let row = |id: &str, parent: Option<&str>, kind| Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: format!("n-{id}"),
        kind,
        size: 0,
        mtime: 0,
        etag: None,
        ctag: None,
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    };
    let mut store = TreeStore::in_memory().unwrap();
    store.begin_staging(NewTree::Whole).unwrap();
    store
        .stage(&[
            Change::Root(row("R", None, Kind::Folder)),
            Change::Upsert(row("D", Some("R"), Kind::Folder)),
            Change::Upsert(row("F", Some("D"), Kind::File)),
            Change::Upsert(row("S", Some("R"), Kind::File)),
        ])
        .unwrap();
    store.commit_staging("link").unwrap();
    store.bench_sql("UPDATE onedrive_items SET placement = 'plbced' WHERE id = 'D'; UPDATE onedrive_items SET placement = 'skipped:nonsense', kind = 'nonsense' WHERE id = 'S'").unwrap();

    assert_eq!(store.get(Table::OneDriveItems, "D").unwrap().unwrap().placement, Placement::Placed);
    assert!(store.locate(Table::OneDriveItems, "D").unwrap().unwrap().placed);
    assert!(store.locate(Table::OneDriveItems, "F").unwrap().unwrap().placed, "what is inside it is in the folder too");
    assert_eq!(store.counts().unwrap(), Counts { listed: 3, placed: 2, skipped: 1 });

    let unknown = store.get(Table::OneDriveItems, "S").unwrap().unwrap();
    assert_eq!((unknown.placement, unknown.kind), (Placement::Skipped(SkipReason::Unsupported), Kind::File));
    assert_eq!(store.skipped().unwrap().into_iter().map(|s| (s.rel, s.reason)).collect::<Vec<_>>(), [(std::path::PathBuf::from("n-S"), SkipReason::Unsupported)]);

    // `INDEXED BY` fails to prepare when the query does not imply the
    // index's own condition.
    store.conn.prepare(&format!("SELECT id FROM onedrive_items INDEXED BY onedrive_items_skipped WHERE {}", skipped("placement"))).unwrap();
}

#[test]
fn a_column_is_found_by_its_name_in_the_list() {
    assert_eq!(column(ROW_COLUMNS, "id"), 0);
    assert_eq!(column(ROW_COLUMNS, "name"), 2);
    assert_eq!(column(ROW_COLUMNS, "placement"), 10);
    assert_eq!(ROW_WIDTH, 11);
    // A name that begins or ends another is not it; a list broken over lines reads the same.
    assert_eq!(column("parent_id, id,\n     name", "id"), 1);
    assert_eq!(column("parent_id, id,\n     name", "name"), 2);
    assert_eq!(width("a,b , c"), 3);
    assert_eq!(width(""), 0);
    assert!(COLUMNS.starts_with(ROW_COLUMNS), "a copied row begins with the columns a row is read by");
}

/// Every row comes back as it went in, through the one decoder: from
/// `onedrive_items`, from `staging`, and from `deferred`.
#[test]
fn a_row_reads_back_as_written_from_every_table() {
    let row = |id: &str, kind, placement| Row {
        id: id.into(),
        parent_id: Some("R".into()),
        name: format!("n-{id}"),
        kind,
        size: 7,
        mtime: -3,
        etag: Some("e".into()),
        ctag: Some("c".into()),
        quickxor: Some("q".into()),
        mime: Some("text/plain".into()),
        placement,
    };
    let root = Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
    let file = row("F", Kind::File, Placement::Placed);
    let skipped = row("S", Kind::Folder, Placement::Skipped(SkipReason::Shared));
    let mut store = crate::TreeStore::in_memory().unwrap();
    store.begin_staging(crate::NewTree::Whole).unwrap();
    store.stage(&[Change::Root(root), Change::Upsert(file.clone()), Change::Upsert(skipped.clone())]).unwrap();
    assert_eq!(store.get(Table::Staging, "F").unwrap(), Some(file.clone()));
    store.commit_staging("link").unwrap();
    assert_eq!(store.get(Table::OneDriveItems, "F").unwrap(), Some(file.clone()));
    assert_eq!(store.get(Table::OneDriveItems, "S").unwrap(), Some(skipped.clone()));
    assert_eq!(store.children(Table::OneDriveItems, "R").unwrap(), vec![file.clone(), skipped]);

    let changed = Row { ctag: Some("c2".into()), size: 9, ..file };
    store.begin_staging(crate::NewTree::Delta).unwrap();
    store.stage(&[Change::Upsert(changed.clone()), Change::Delete("S".into())]).unwrap();
    store.commit_staging_deferring("link-2", &crate::reconcile::Deferrals { consumed: &[], whole: &["F".into(), "S".into()], content: &[], fetched_at: 1, waits: &[] }).unwrap();
    assert_eq!(store.deferred("F").unwrap(), Some(Change::Upsert(changed)));
    assert_eq!(store.deferred("S").unwrap(), Some(Change::Delete("S".into())));
}

/// One drive's id is one `DriveId` however its letters' case is spelled, where it is made:
/// in a comparison, as a key, as text, and against a text spelled another way. Nothing but
/// the case of ASCII letters is changed: a spelling of another length is another id.
#[test]
fn a_drive_id_is_the_same_in_either_letter_case() {
    let (upper, lower) = (DriveId::new("B!AbC9D1E2").unwrap(), DriveId::new("b!abc9d1e2").unwrap());
    assert_eq!(upper, lower);
    assert_eq!(upper.as_str(), "b!abc9d1e2");
    assert_eq!(upper.as_str(), lower.clone().into_string());
    assert_eq!(std::collections::HashSet::from([upper.clone(), lower.clone()]).len(), 1);
    assert!(upper == "B!ABC9D1E2" && upper == *"b!abc9d1e2" && upper == "B!AbC9D1E2".to_owned());
    assert!(upper != "b!abc9d1e3");
    assert_ne!(DriveId::new("0abc9d1e2"), DriveId::new("abc9d1e2"), "a leading zero is not taken off");
    assert_eq!(DriveId::new(""), None);
}

fn file_item(onedrive_item: bool, presence: ContentPresence, local: &str, synced: Option<&str>) -> Item {
    let content = |hash: &str| ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: None };
    let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "file".into() };
    Item {
        local_item_id: LocalItemId::new(2),
        onedrive_item: onedrive_item.then(|| OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new("A") }),
        kind: ItemKind::File,
        local: Some(ItemVersion { place: Some(place.clone()), content: Some(content(local)) }),
        onedrive: None,
        onedrive_etag: None,
        synced_place: synced.map(|_| place),
        synced_content: synced.map(content),
        content_presence: presence,
        content_before_write: None,
        placement: crate::model::Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
    }
}

/// The sync state of a file, a case each, and the first that holds: no binding is
/// `LocalOnly` whatever else; content not on disk is `RemoteOnly` whatever the contents;
/// local content that is not the synced one is `ModifiedHere`; `Synced` otherwise.
#[test]
fn the_sync_state_of_a_file_is_the_first_case_that_holds() {
    use ContentPresence::{NotOnDisk, OnDisk};
    assert_eq!(file_item(false, OnDisk, "new", None).file_sync_state(), Some(FileSyncState::LocalOnly));
    assert_eq!(file_item(false, NotOnDisk, "old", Some("old")).file_sync_state(), Some(FileSyncState::LocalOnly), "no binding comes first");
    assert_eq!(file_item(true, NotOnDisk, "old", Some("old")).file_sync_state(), Some(FileSyncState::RemoteOnly));
    assert_eq!(file_item(true, NotOnDisk, "new", Some("old")).file_sync_state(), Some(FileSyncState::RemoteOnly), "not on disk comes before the contents");
    assert_eq!(file_item(true, OnDisk, "new", Some("old")).file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(file_item(true, OnDisk, "new", None).file_sync_state(), Some(FileSyncState::ModifiedHere), "bound, and no content agreed on yet");
    assert_eq!(file_item(true, OnDisk, "old", Some("old")).file_sync_state(), Some(FileSyncState::Synced));

    // A content a program changed is not the hashed one of the same size and time.
    let mut written = file_item(true, OnDisk, "old", Some("old"));
    written.local.as_mut().unwrap().content = Some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 1 });
    assert_eq!(written.file_sync_state(), Some(FileSyncState::ModifiedHere));
    // A move alone changes no content.
    let mut moved = file_item(true, OnDisk, "old", Some("old"));
    moved.local.as_mut().unwrap().place.as_mut().unwrap().name = "moved".into();
    assert_eq!(moved.file_sync_state(), Some(FileSyncState::Synced));
}

/// Only a file the folder shows has a file's sync state: a directory and an item the
/// folder does not show have none.
#[test]
fn only_a_file_the_folder_shows_has_a_file_sync_state() {
    let mut directory = file_item(true, ContentPresence::NotOnDisk, "x", None);
    directory.kind = ItemKind::Directory;
    directory.local = Some(ItemVersion { place: directory.local.unwrap().place, content: None });
    assert_eq!(directory.file_sync_state(), None);
    assert_eq!(Item { local: None, ..file_item(true, ContentPresence::NotOnDisk, "x", Some("x")) }.file_sync_state(), None);
}

/// The words `items` keeps for a kind and for whether content is on disk, spelled out; a
/// presence that cannot be read is not on disk.
#[test]
fn the_words_of_an_item_are_spelled_as_stored() {
    assert_eq!((ItemKind::File.as_str(), ItemKind::Directory.as_str()), ("file", "directory"));
    assert_eq!((ItemKind::decode("directory"), ItemKind::decode("file")), (ItemKind::Directory, ItemKind::File));
    assert_eq!((ContentPresence::OnDisk.as_str(), ContentPresence::NotOnDisk.as_str()), ("on-disk", "not-on-disk"));
    assert_eq!([ContentPresence::decode("on-disk"), ContentPresence::decode("not-on-disk"), ContentPresence::decode("0n-disk")], [ContentPresence::OnDisk, ContentPresence::NotOnDisk, ContentPresence::NotOnDisk]);
}
