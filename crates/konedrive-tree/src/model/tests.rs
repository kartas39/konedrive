use super::*;

/// The words the store keeps, spelled out: a store written by another
/// build reads the same, so a constant reworded fails here.
#[test]
fn the_stored_words_are_spelled_as_they_always_were() {
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
        onedrive_mime: None,
        synced_place: synced.map(|_| place),
        synced_content: synced.map(content),
        content_presence: presence,
        content_before_write: None,
        placement: crate::model::Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
        is_executable: false,
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
