//! Make: the decision's cases, and each making over the fixture.

use std::ffi::OsStr;

use konedrive_tree::{ContentPresence, ContentVersion, FileSyncState};

use super::*;
use crate::folder::running_steps::{RunningStep, StepClass};
use crate::fuse::item_fixture::{onedrive_id_of, ItemFixture};
use crate::fuse::item_write_operations::tests::{bind, make_row, OperationPoint, OperationsFixture};

/// A make of `kind` in a directory the folder shows, at a name nothing has.
fn case(kind: MadeKind) -> MakeCase {
    MakeCase {
        directory: Some(make_row(2, ItemKind::Directory)),
        directory_path: Some("d".into()),
        directory_marks: vec![None],
        holder: None,
        removed_file: None,
        has_removed_file_a_running_step: false,
        kind,
        is_hidden_name: false,
        is_trash: false,
        is_name_ignored: false,
    }
}

#[test]
fn each_case_of_a_make_is_decided() {
    let removed = bind(make_row(9, ItemKind::File));
    for kind in [MadeKind::Directory, MadeKind::File, MadeKind::FileOpenedForWriting] {
        assert_eq!(decide_make(&case(kind)), Ok(MakeDecision { is_name_ignored: false, replaced: None }), "{kind:?}: a free name");
        assert_eq!(decide_make(&MakeCase { is_name_ignored: true, ..case(kind) }), Ok(MakeDecision { is_name_ignored: true, replaced: None }), "{kind:?}: an ignored name is made, with the flag");
        assert_eq!(decide_make(&MakeCase { directory: None, ..case(kind) }), Err(Errno::ENOENT), "{kind:?}: no directory");
        assert_eq!(decide_make(&MakeCase { is_hidden_name: true, ..case(kind) }), Err(Errno::EPERM), "{kind:?}: a name the mount hides");
        assert_eq!(decide_make(&MakeCase { is_trash: true, ..case(kind) }), Err(Errno::EPERM), "{kind:?}: the trash");
        assert_eq!(decide_make(&MakeCase { holder: Some(make_row(5, ItemKind::File)), ..case(kind) }), Err(Errno::EEXIST), "{kind:?}: a taken name");
        // Today's order: no directory, then the name, then what holds it.
        assert_eq!(decide_make(&MakeCase { directory: None, is_hidden_name: true, holder: Some(make_row(5, ItemKind::File)), ..case(kind) }), Err(Errno::ENOENT));
        assert_eq!(decide_make(&MakeCase { is_hidden_name: true, holder: Some(make_row(5, ItemKind::File)), ..case(kind) }), Err(Errno::EPERM));
    }
    // A file takes over the binding of the file removed there; a directory never does.
    for kind in [MadeKind::File, MadeKind::FileOpenedForWriting] {
        let over_removed = MakeCase { removed_file: Some(removed.clone()), ..case(kind) };
        assert_eq!(decide_make(&over_removed), Ok(MakeDecision { is_name_ignored: false, replaced: Some(removed.local_item_id) }), "{kind:?}");
        assert_eq!(decide_make(&MakeCase { has_removed_file_a_running_step: true, ..over_removed.clone() }).unwrap().replaced, None, "{kind:?}: its removal is being sent");
        assert_eq!(decide_make(&MakeCase { is_name_ignored: true, ..over_removed }), Ok(MakeDecision { is_name_ignored: true, replaced: None }), "{kind:?}: a file that is left here takes nothing over");
    }
    assert_eq!(decide_make(&MakeCase { removed_file: Some(removed), ..case(MadeKind::Directory) }).unwrap().replaced, None);
}

#[test]
fn a_directory_made_is_on_disk_and_then_in_its_row() {
    let operations = OperationsFixture::new();
    // With the disk changed and the row not written: the directory is there, no row is.
    let (on_disk, store) = (operations.fixture.disk_path("made"), operations.fixture.store.clone());
    operations.run_at(OperationPoint::DiskChanged, move || {
        assert!(on_disk.is_dir(), "the directory is on disk before its row");
        let row = store.call_blocking(|store| store.item_store().reader().find_item_at(LocalItemId::ROOT, OsStr::new("made"))).unwrap();
        assert_eq!(row, None, "no row before the disk");
    });
    let made = operations.make("made", MadeKind::Directory).unwrap();
    assert_eq!(made.fh, Fh::NOT_OPENED);
    assert_eq!(Some(&made.item), operations.fixture.find_item_at("made").as_ref(), "the row that was written is answered");
    assert_eq!((made.item.kind, made.item.onedrive_item.clone(), made.item.is_name_ignored), (ItemKind::Directory, None, false));
    assert!(made.item.unsent_since.is_some(), "a new directory is unsent");
    assert!(operations.fixture.disk_path("made").is_dir());
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_made_below_a_directory_that_is_not_on_disk_makes_it_marked() {
    let operations = OperationsFixture::new();
    operations.fixture.show_onedrive_directory("docs");
    operations.fixture.show_onedrive_directory("docs/deep");
    assert!(!operations.fixture.disk_path("docs").exists());
    operations.make("docs/deep/made", MadeKind::Directory).unwrap();
    assert!(operations.fixture.disk_path("docs/deep/made").is_dir());
    assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, Some(onedrive_id_of("docs")));
    assert_eq!(operations.fixture.read_disk_marks("docs/deep").onedrive_item_id, Some(onedrive_id_of("docs/deep")));
    assert_eq!(operations.fixture.read_disk_marks("docs/deep/made").onedrive_item_id, None, "a new directory carries no mark");
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_made_is_empty_on_disk_changed_here_and_on_disk_in_its_row() {
    let operations = OperationsFixture::new();
    let made = operations.make("made.txt", MadeKind::File).unwrap();
    assert_eq!(made.fh, Fh::NOT_OPENED);
    assert_eq!(operations.fixture.read_disk_file("made.txt").as_deref(), Some(&b""[..]));
    let row = operations.fixture.find_item_at("made.txt").unwrap();
    assert_eq!(row, made.item);
    assert_eq!((row.content_presence, row.file_sync_state(), row.content_before_write.clone()), (ContentPresence::OnDisk, Some(FileSyncState::LocalOnly), None));
    // The content is the backing file's: size 0, its time, changed here once.
    let on_disk = std::fs::metadata(operations.fixture.disk_path("made.txt")).unwrap();
    let (mtime, mtime_nsec) = { use std::os::unix::fs::MetadataExt; (on_disk.mtime(), on_disk.mtime_nsec() as u32) };
    assert_eq!(row.local.unwrap().content, Some(ContentVersion::ChangedHere { size: 0, mtime, mtime_nsec, change_count: 1 }));
    assert!(!operations.fixture.item_handles.is_open_for_writing(row.local_item_id));
    assert_eq!(operations.list_records(), []);
    operations.fixture.assert_rows_and_disk_agree();
}

/// A file of OneDrive at `path` with its copy on disk, removed here: a removal to send.
fn remove_synced_file(operations: &OperationsFixture, path: &str) -> LocalItemId {
    let copy = operations.fixture.show_onedrive_file(path, b"as it was").local_item_id;
    operations.fixture.land_copy(copy, b"as it was");
    operations.remove(path, ItemKind::File).unwrap();
    assert!(operations.fixture.find_item(copy).is_some_and(|row| row.local.is_none() && row.onedrive_item.is_some()), "the removal is not sent");
    copy
}

#[test]
fn a_file_made_where_a_file_was_removed_and_not_sent_takes_its_binding_over() {
    let operations = OperationsFixture::new();
    let removed = remove_synced_file(&operations, "doc.txt");
    let made = operations.make("doc.txt", MadeKind::File).unwrap();
    assert_ne!(made.item.local_item_id, removed, "the made file is another item");
    assert_eq!(operations.fixture.find_item(removed), None, "the row of the removed file went");
    // OneDrive sees one item whose content changed: the binding, what OneDrive has and
    // what was agreed on are the made file's.
    assert_eq!(made.item.onedrive_item, Some(ItemFixture::onedrive_reference_of("doc.txt")));
    assert_eq!(made.item.synced_content, Some(ItemFixture::onedrive_content_of(b"as it was")));
    assert_eq!(made.item.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(operations.fixture.list_items().iter().filter(|row| row.kind == ItemKind::File).count(), 1);
    // On disk: the new, empty file, marked as that item's.
    assert_eq!(operations.fixture.read_disk_file("doc.txt").as_deref(), Some(&b""[..]));
    assert_eq!(operations.fixture.read_disk_marks("doc.txt").onedrive_item_id, Some(onedrive_id_of("doc.txt")));
    assert_eq!((operations.list_records(), operations.list_aside_entries()), (vec![], vec![]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_made_where_a_removal_is_being_sent_takes_nothing_over() {
    let operations = OperationsFixture::new();
    let removed = remove_synced_file(&operations, "doc.txt");
    assert!(operations.fixture.running_steps.begin(RunningStep { class: StepClass::Metadata, item: removed, other_items: vec![], destination: None, sent: 0, total: 0 }));
    let made = operations.make("doc.txt", MadeKind::FileOpenedForWriting).unwrap();
    assert_eq!(made.item.onedrive_item, None, "a new item");
    assert!(operations.fixture.find_item(removed).is_some_and(|row| row.local.is_none() && row.onedrive_item.is_some()), "the removal stands");
    assert_eq!(operations.fixture.read_disk_marks("doc.txt").onedrive_item_id, None);
    operations.close(made.fh);
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_name_the_upload_rules_ignore_is_made_with_the_flag() {
    let operations = OperationsFixture::new();
    let (cache, file) = (operations.make("cache", MadeKind::Directory).unwrap(), operations.make("draft.tmp", MadeKind::File).unwrap());
    assert!(cache.item.is_name_ignored && file.item.is_name_ignored);
    assert!(!operations.make("kept.txt", MadeKind::File).unwrap().item.is_name_ignored);
    // A file made in an ignored directory has no flag of its own: the rows say it is left here.
    let inside = operations.make("cache/inside.txt", MadeKind::File).unwrap();
    assert!(!inside.item.is_name_ignored);
    let id = inside.item.local_item_id;
    assert!(operations.fixture.read_rows(move |reader| reader.is_ignored(id)));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_name_of_the_daemons_own_and_the_trash_are_refused() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_directory("d");
    for name in [".konedrive-holding", ".konedrive-new-7", ".konedrive-aside-7", ".konedrive-swap-7", "d/.konedrive-aside-7"] {
        for kind in [MadeKind::Directory, MadeKind::File, MadeKind::FileOpenedForWriting] {
            assert_eq!(operations.make(name, kind).map(|made| made.fh), Err(Errno::EPERM), "{name} as {kind:?}");
        }
        assert!(!operations.fixture.disk_path(name).exists());
    }
    // A file manager's trash directory in the folder itself; not a file of that name, and
    // not a directory of that name further down.
    assert_eq!(operations.make(".Trash-1000", MadeKind::Directory).map(|made| made.fh), Err(Errno::EPERM));
    assert!(operations.make(".Trash-1001", MadeKind::File).is_ok());
    assert!(operations.make("d/.Trash-1000", MadeKind::Directory).is_ok());
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_name_that_is_taken_is_refused_before_the_disk_is_touched() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_file("taken.txt", b"mine");
    // Also by a file that is not on disk: the rows say the name, not the disk.
    operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive");
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&reached);
    operations.run_at(OperationPoint::RecordWritten, move || seen.store(true, std::sync::atomic::Ordering::SeqCst));
    for name in ["taken.txt", "remote.txt"] {
        for kind in [MadeKind::Directory, MadeKind::File, MadeKind::FileOpenedForWriting] {
            assert_eq!(operations.make(name, kind).map(|made| made.fh), Err(Errno::EEXIST), "{name} as {kind:?}");
        }
    }
    assert!(!reached.load(std::sync::atomic::Ordering::SeqCst), "no record was written: nothing was begun");
    assert_eq!(operations.fixture.read_disk_file("taken.txt").as_deref(), Some(&b"mine"[..]));
    assert!(!operations.fixture.disk_path("remote.txt").exists());
    // A node that is no directory the folder shows.
    let file = operations.id("taken.txt");
    assert_eq!(operations.run(move |operations| operations.make(file, OsStr::new("x"), MadeKind::File)).map(|made| made.fh), Err(Errno::ENOENT));
    assert_eq!(operations.run(|operations| operations.make(LocalItemId::new(999), OsStr::new("x"), MadeKind::File)).map(|made| made.fh), Err(Errno::ENOENT));
}

#[test]
fn a_name_the_disk_holds_though_no_row_says_so_is_refused_and_nothing_is_replaced() {
    let operations = OperationsFixture::new();
    operations.fixture.write_disk_file("leftover.txt", b"what no row says");
    for kind in [MadeKind::File, MadeKind::FileOpenedForWriting] {
        assert_eq!(operations.make("leftover.txt", kind).map(|made| made.fh), Err(Errno::EEXIST), "{kind:?}");
    }
    assert_eq!(operations.fixture.read_disk_file("leftover.txt").as_deref(), Some(&b"what no row says"[..]));
    assert_eq!(operations.fixture.find_item_at("leftover.txt"), None);
    assert_eq!(operations.list_records(), [], "the record of a change the disk refused is taken");
    assert_eq!(operations.take_downs(), []);
    // A directory that is on disk already is taken as the made one, with what it holds.
    std::fs::create_dir(operations.fixture.disk_path("kept")).unwrap();
    operations.fixture.write_disk_file("kept/inside.txt", b"what no row says");
    operations.make("kept", MadeKind::Directory).unwrap();
    assert_eq!(operations.fixture.read_disk_file("kept/inside.txt").as_deref(), Some(&b"what no row says"[..]));
    assert_eq!(operations.list_records(), []);
}

#[test]
fn a_file_created_is_made_and_open_for_writing_in_one_transaction() {
    let operations = OperationsFixture::new();
    let made = operations.make("created.txt", MadeKind::FileOpenedForWriting).unwrap();
    let id = made.item.local_item_id;
    // The row is that of a file whose first writer opened it: the content of its making
    // kept, and another content since.
    let ContentVersion::ChangedHere { size: 0, mtime, mtime_nsec, change_count: 1 } = made.item.content_before_write.clone().unwrap() else { panic!("the content of the making: {:?}", made.item) };
    assert_eq!(made.item.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 0, mtime, mtime_nsec, change_count: 2 }));
    assert_eq!(made.item.content_presence, ContentPresence::OnDisk);
    assert_ne!(made.fh, Fh::NOT_OPENED);
    assert!(operations.fixture.item_handles.is_open_for_writing(id));
    assert_eq!(operations.list_records(), [], "the record went in the row's transaction");
    // What is written through the handle is the made file's content at its close.
    operations.write_through(made.fh, b"written");
    operations.close(made.fh);
    assert_eq!(operations.fixture.read_disk_file("created.txt").as_deref(), Some(&b"written"[..]));
    let closed = operations.fixture.find_item(id).unwrap();
    assert_eq!((closed.local.unwrap().content.unwrap().size(), closed.content_before_write), (7, None));
    operations.fixture.assert_rows_and_disk_agree();
    // One that is closed with nothing written is the empty file of its making.
    let untouched = operations.make("untouched.txt", MadeKind::FileOpenedForWriting).unwrap();
    operations.close(untouched.fh);
    let row = operations.fixture.find_item(untouched.item.local_item_id).unwrap();
    assert_eq!((row.local.as_ref().unwrap().content.clone(), row.content_before_write.clone()), (untouched.item.content_before_write, None));
    assert!(row.unsent_since.is_some(), "an empty new file is unsent");
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_row_refused_after_the_disk_removes_what_was_made() {
    for kind in [MadeKind::File, MadeKind::FileOpenedForWriting] {
        let operations = OperationsFixture::new();
        operations.fixture.show_onedrive_directory("docs");
        operations.take_place_at(OperationPoint::DiskChanged, "docs", "made.txt");
        assert_eq!(operations.make("docs/made.txt", kind).map(|made| made.fh), Err(Errno::EEXIST), "{kind:?}");
        // The file is gone, and the directory that was made above it with it.
        assert!(!operations.fixture.disk_path("docs/made.txt").exists() && !operations.fixture.disk_path("docs").exists(), "{kind:?}");
        assert_eq!((operations.list_records(), operations.take_downs()), (vec![], vec![]), "{kind:?}");
    }
}

#[test]
fn a_made_entry_that_cannot_be_removed_again_asks_for_the_take_down_and_keeps_its_record() {
    let operations = OperationsFixture::new();
    operations.take_place_at(OperationPoint::DiskChanged, "", "made.txt");
    // The made file's directory loses its write bit before the undo: it cannot be removed.
    // (Run as a user the mode binds; root would remove it all the same.)
    let folder = operations.fixture.folder();
    let (store, root) = (operations.fixture.store.clone(), LocalItemId::ROOT);
    operations.run_at(OperationPoint::DiskChanged, move || {
        use std::os::unix::fs::PermissionsExt;
        let place = konedrive_tree::ItemPlace { parent_local_item_id: root, name: "made.txt".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
    });
    let made = operations.make("made.txt", MadeKind::File).map(|made| made.fh);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(operations.fixture.folder(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(made, Err(Errno::EIO));
    assert_eq!(operations.take_downs(), [crate::fuse::mount_take_down::TakeDownCause::UndoFailed { path: "made.txt".into() }]);
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::MakeEntry { kind: ItemKind::File, .. }]));
    assert!(operations.fixture.disk_path("made.txt").is_file(), "what could not be removed is still there, for the check at start");
}

/// The review's S3: what `make` answers comes out of the job that wrote the row, and a
/// created file has its writer whatever the rows say a moment later. Here the row is
/// gone again by the time a second read would be made.
#[test]
fn a_created_file_is_answered_and_has_its_writer_whatever_is_read_after_its_row() {
    let operations = OperationsFixture::new();
    let store = operations.fixture.store.clone();
    operations.run_at(OperationPoint::RowWritten, move || {
        store
            .call_blocking(|store| {
                let made = store.item_store().reader().find_item_at(LocalItemId::ROOT, OsStr::new("created.txt"))?.expect("the row is written");
                store.item_store().apply(&LocalVersionChange::Removed { item: made.local_item_id }).map(drop)
            })
            .unwrap();
    });
    let made = operations.make("created.txt", MadeKind::FileOpenedForWriting).expect("the file was made, and its row written");
    assert!(made.item.local.is_some() && made.item.content_before_write.is_some(), "the row as it was written: {:?}", made.item);
    assert_ne!(made.fh, Fh::NOT_OPENED);
    assert!(operations.fixture.item_handles.is_open_for_writing(made.item.local_item_id), "the writer the row was written for is among the handles");
}
