//! What every write operation shares — the errnos, the undo of a row that is refused, the
//! take-down, the wake, the access mode, a reader beside an operation, a death at each
//! point — and what the tests of each operation are made with.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_tree::{ContentPresence, ContentVersion, ItemKind, ItemPlace, ItemVersion, LocalVersionChange, OneDriveItemId, OneDriveItemReference, Placement, RowRefusal, RowRefusalReason};

use super::*;
use crate::mount::item_fixture::{onedrive_id_of, ItemFixture, DRIVE_ID};
use crate::mount::mount_take_down::TakeDownCause;
use crate::mount::place_changes::OpenMode;

/// What a test has run at a point of an operation.
pub(super) type Hook = Arc<dyn Fn(OperationPoint) + Send + Sync>;

/// The hook of one `ItemWriteOperations`: none until a test sets one.
#[derive(Default)]
pub(super) struct OperationPointHook(Mutex<Option<Hook>>);

/// Where an operation that changes a name stands between its record and its row. A
/// content operation that makes a backing file writes its row before the file: it reaches
/// `RowWritten` before `DiskChanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OperationPoint {
    /// The record is in the store, and the disk is as it was.
    RecordWritten,
    /// The disk is changed.
    DiskChanged,
    /// The row is written.
    RowWritten,
}

const POINTS: [OperationPoint; 3] = [OperationPoint::RecordWritten, OperationPoint::DiskChanged, OperationPoint::RowWritten];

impl ItemWriteOperations {
    /// The operation is at `point`: the test's hook runs, on the write-operation thread.
    pub(super) fn reach(&self, point: OperationPoint) {
        let hook = crate::panic::lock(&self.operation_point_hook.0).clone();
        if let Some(hook) = hook {
            hook(point);
        }
    }
}

impl ItemWriteOperations {
    /// `hook` runs where an operation changed the disk and has not written its row, on
    /// the write-operation thread: for the tests of what answers the kernel, which change
    /// a row there as nothing under the tree lock can, so that the store refuses the
    /// operation's row.
    pub(in crate::mount) fn run_before_row(&self, hook: impl Fn() + Send + Sync + 'static) {
        let hook: Hook = Arc::new(move |reached| {
            if reached == OperationPoint::DiskChanged {
                hook();
            }
        });
        *crate::panic::lock(&self.operation_point_hook.0) = Some(hook);
    }
}

/// The fixture with the write operations over its leaves.
pub(super) struct OperationsFixture {
    pub(super) fixture: ItemFixture,
    pub(super) operations: Arc<ItemWriteOperations>,
}

/// A row with nothing but its id and kind said: what a decision's case is made of.
pub(super) fn make_row(id: i64, kind: ItemKind) -> Item {
    let content = (kind == ItemKind::File).then_some(ContentVersion::ChangedHere { size: 3, mtime: 1_700_000_000, mtime_nsec: 0, change_count: 1 });
    Item {
        local_item_id: LocalItemId::new(id),
        onedrive_item: None,
        kind,
        local: Some(ItemVersion { place: Some(ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("item-{id}").into() }), content }),
        onedrive: None,
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: None,
        synced_content: None,
        content_presence: if kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk },
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
    }
}

/// `row` as an item OneDrive has.
pub(super) fn bind(mut row: Item) -> Item {
    row.onedrive_item = Some(OneDriveItemReference { drive_id: konedrive_tree::DriveId::new(DRIVE_ID).unwrap(), item_id: OneDriveItemId::new(format!("id-{}", row.local_item_id)) });
    row
}

/// `row` as a file whose content is not on disk.
pub(super) fn take_off_disk(mut row: Item) -> Item {
    row.content_presence = ContentPresence::NotOnDisk;
    row
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

impl OperationsFixture {
    pub(super) fn new() -> Self {
        let fixture = ItemFixture::new();
        let operations = Arc::new(ItemWriteOperations::new(
            Arc::clone(&fixture.place_changes),
            Arc::clone(&fixture.backing_directory),
            fixture.store.clone(),
            Arc::clone(&fixture.read_connections),
            Arc::clone(&fixture.item_handles),
            Arc::clone(&fixture.download_states),
            Arc::clone(&fixture.upload_rules),
            Arc::clone(&fixture.access_mode_switch),
            Arc::clone(&fixture.running_steps),
            Arc::clone(&fixture.sync_plan_wake),
            Arc::clone(&fixture.mount_take_down),
        ));
        OperationsFixture { fixture, operations }
    }

    /// Runs `job` over the operations on the write-operation thread, as a request's job.
    pub(super) fn run<T: Send + 'static>(&self, job: impl FnOnce(&ItemWriteOperations) -> T + Send + 'static) -> T {
        let operations = Arc::clone(&self.operations);
        self.fixture.run_on_write_operation_thread(move || job(&operations))
    }

    /// The id of what the folder shows at `path`.
    pub(super) fn id(&self, path: &str) -> LocalItemId {
        self.fixture.find_item_at(path).unwrap_or_else(|| panic!("the folder shows nothing at {path:?}")).local_item_id
    }

    pub(super) fn make(&self, path: &str, kind: MadeKind) -> Result<MadeItem, Errno> {
        let (directory, name) = split(path);
        let (directory, name) = (self.id(directory), name.to_owned());
        self.run(move |operations| operations.make(directory, OsStr::new(&name), kind))
    }

    pub(super) fn rename_as(&self, from: &str, to: &str, mode: RenameMode) -> Result<(), Errno> {
        let ((directory, name), (new_directory, new_name)) = (split(from), split(to));
        let (directory, name, new_directory, new_name) = (self.id(directory), name.to_owned(), self.id(new_directory), new_name.to_owned());
        self.run(move |operations| operations.rename(directory, OsStr::new(&name), new_directory, OsStr::new(&new_name), mode))
    }

    pub(super) fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.rename_as(from, to, RenameMode::Replace)
    }

    pub(super) fn remove(&self, path: &str, asked_kind: ItemKind) -> Result<(), Errno> {
        let (directory, name) = split(path);
        let (directory, name) = (self.id(directory), name.to_owned());
        self.run(move |operations| operations.remove(directory, OsStr::new(&name), asked_kind))
    }

    /// The records of place changes that stand.
    pub(super) fn list_records(&self) -> Vec<PlaceChangeRecord> {
        self.fixture.write_rows(|store| store.item_store().list_place_change_records()).into_iter().map(|(_, record)| record).collect()
    }

    /// The names of the entries set aside in the root of the backing directory.
    pub(super) fn list_aside_entries(&self) -> Vec<String> {
        let names = std::fs::read_dir(self.fixture.folder()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned());
        names.filter(|name| name.starts_with(crate::folder::disk::ASIDE_ENTRY_PREFIX)).collect()
    }

    /// `hook` runs when an operation is at `point`, on the write-operation thread.
    pub(super) fn run_at(&self, point: OperationPoint, hook: impl Fn() + Send + Sync + 'static) {
        let hook: Hook = Arc::new(move |reached| {
            if reached == point {
                hook();
            }
        });
        *crate::panic::lock(&self.operations.operation_point_hook.0) = Some(hook);
    }

    pub(super) fn clear_hook(&self) {
        *crate::panic::lock(&self.operations.operation_point_hook.0) = None;
    }

    /// The daemon dies when an operation is at `point`: the job panics there, which leaves
    /// the store and the disk as a death does, and ends the place change.
    pub(super) fn die_at(&self, point: OperationPoint) {
        self.run_at(point, move || panic!("the daemon dies at {point:?}"));
    }

    /// Runs `operation`; whether the daemon died in it.
    pub(super) fn run_until_death(&self, operation: impl FnOnce(&OperationsFixture)) -> bool {
        let died = catch_unwind(AssertUnwindSafe(|| operation(self))).is_err();
        self.clear_hook();
        died
    }

    /// A row takes the place `name` in `directory` behind the operation's back, as nothing
    /// under the tree lock can: what makes the store refuse the operation's row.
    pub(super) fn take_place_at(&self, point: OperationPoint, directory: &str, name: &str) {
        let (store, place) = (self.fixture.store.clone(), ItemPlace { parent_local_item_id: self.id(directory), name: name.into() });
        self.run_at(point, move || {
            let place = place.clone();
            store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
        });
    }

    /// Whether a cause was given to the plan's wake since it was last asked; it is taken.
    pub(super) fn take_wake(&self) -> bool {
        take_wake(&self.fixture.runtime(), &self.fixture.sync_plan_wake)
    }

    /// The writer `fh` writes `bytes` into its file from the start, as the request does.
    pub(super) fn write_through(&self, fh: crate::mount::item_handles::Fh, bytes: &[u8]) {
        use std::os::unix::fs::FileExt;
        let (item, backing_file) = self.fixture.item_handles.find_file_to_write(fh).expect("a writer");
        self.fixture.item_handles.mark_written(item);
        backing_file.write_all_at(bytes, 0).unwrap();
    }

    pub(super) fn close(&self, fh: crate::mount::item_handles::Fh) {
        self.run(move |operations| operations.close_writer(fh));
    }

    pub(super) fn take_downs(&self) -> Vec<TakeDownCause> {
        crate::panic::lock(&self.fixture.take_downs).clone()
    }
}

fn take_wake(runtime: &tokio::runtime::Handle, wake: &Arc<SyncPlanWake>) -> bool {
    let wake = Arc::clone(wake);
    runtime.block_on(async move { tokio::time::timeout(Duration::ZERO, wake.wait()).await.is_ok() })
}

#[test]
fn a_refusal_of_the_store_is_the_errno_of_its_rule() {
    let refused = |reason| TreeError::Refused(RowRefusal { reason, text: "the text".into() });
    // What `mkdir -p`, `rm -r`, `mv` and a save by rename go on by when two programs race.
    assert_eq!(decide_store_errno(&refused(RowRefusalReason::ItemGone)), Errno::ESTALE);
    assert_eq!(decide_store_errno(&refused(RowRefusalReason::PlaceTaken)), Errno::EEXIST);
    assert_eq!(decide_store_errno(&refused(RowRefusalReason::DirectoryNotEmpty)), Errno::ENOTEMPTY);
    assert_eq!(decide_store_errno(&refused(RowRefusalReason::NoDirectory)), Errno::ENOENT);
    // A rule no program is answered by, and a real failure.
    assert_eq!(decide_store_errno(&refused(RowRefusalReason::AnotherRule)), Errno::EIO);
    assert_eq!(decide_store_errno(&TreeError::Refused("a writer holds item 7".into())), Errno::EIO);
    assert_eq!(decide_store_errno(&TreeError::Io(io::Error::from_raw_os_error(libc::ENOSPC))), Errno::EIO);
    assert_eq!(decide_store_errno(&TreeError::Sql(rusqlite::Error::InvalidQuery)), Errno::EIO);
    assert_eq!(decide_store_errno(&TreeError::Schema(None)), Errno::EIO);
}

#[test]
fn a_place_held_on_disk_is_eexist_and_any_other_refusal_the_disks_own_errno() {
    assert_eq!(decide_place_errno(&PlaceRefusal::PlaceHeld { path: "a/b".into() }), Errno::EEXIST);
    assert_eq!(decide_place_errno(&PlaceRefusal::Disk { errno: libc::ENOSPC, path: "a/b".into() }), Errno::ENOSPC);
    assert_eq!(decide_place_errno(&PlaceRefusal::Disk { errno: libc::ENOENT, path: "a/b".into() }), Errno::ENOENT);
    assert_eq!(decide_place_errno(&PlaceRefusal::NotPutBack { errno: libc::ENOSPC, path: "a/b".into() }), Errno::EIO);
}

/// The review's S1: an entry that was set aside for a move and could not be put back
/// when the move failed is at its aside name, with a row that says it is at its place.
/// Its record must stand — the check at start puts the entry back from it, and what is
/// still set aside with no record is removed at the next start — and the mount go down.
#[test]
fn a_change_refused_with_an_entry_that_could_not_be_put_back_keeps_its_record_and_asks_for_the_take_down() {
    let operations = OperationsFixture::new();
    let displaced = operations.fixture.make_own_file("doc.txt", b"the user's file").local_item_id;
    let moved = operations.fixture.make_own_file("doc.new", b"saved").local_item_id;
    let record = move |operations: &ItemWriteOperations| {
        let place = |name: &str| ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.into() };
        operations.write_place_change_record(PlaceChangeRecord::MoveEntryOver { item: moved, from: place("doc.new"), to: place("doc.txt"), replaced: displaced, aside_name: crate::folder::disk::make_aside_entry_name(displaced) }).unwrap()
    };
    // A refusal that left the disk as it was: the record goes, and nothing else happens.
    let errno = operations.run(move |operations| operations.refuse_place_change(record(operations), &PlaceRefusal::Disk { errno: libc::ENOSPC, path: "doc.txt".into() }));
    assert_eq!((errno, operations.list_records().len(), operations.take_downs()), (Errno::ENOSPC, 0, vec![]));
    // One that left the displaced entry aside.
    let errno = operations.run(move |operations| operations.refuse_place_change(record(operations), &PlaceRefusal::NotPutBack { errno: libc::ENOSPC, path: "doc.txt".into() }));
    assert_eq!(errno, Errno::EIO);
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::MoveEntryOver { replaced, .. }] if replaced == displaced), "the record stands for the check at start: {:?}", operations.list_records());
    assert_eq!(operations.take_downs(), [TakeDownCause::UndoFailed { path: "doc.txt".into() }]);
    assert!(!operations.fixture.access_mode_switch.is_read_write(), "the folder takes no further change");
}

#[test]
fn a_failed_row_is_undone_on_disk_and_the_program_gets_the_errno() {
    let operations = OperationsFixture::new();
    operations.take_place_at(OperationPoint::DiskChanged, "", "made");
    let made = operations.make("made", MadeKind::Directory);
    assert_eq!(made.map(|made| made.item.local_item_id), Err(Errno::EEXIST));
    assert!(!operations.fixture.disk_path("made").exists(), "the directory that was made is removed again");
    assert_eq!(operations.list_records(), [], "the record went with the undo");
    assert_eq!(operations.take_downs(), []);
    assert!(operations.fixture.access_mode_switch.is_read_write());
    // The next operation is taken: no place change was left open.
    operations.clear_hook();
    assert!(operations.make("other", MadeKind::Directory).is_ok());
}

#[test]
fn an_undo_that_fails_asks_for_the_mounts_take_down() {
    let operations = OperationsFixture::new();
    // The row is refused, and the made directory holds something by then: it cannot be
    // removed again.
    let (store, root, in_the_way) = (operations.fixture.store.clone(), LocalItemId::ROOT, operations.fixture.disk_path("made/in-the-way"));
    operations.run_at(OperationPoint::DiskChanged, move || {
        let place = ItemPlace { parent_local_item_id: root, name: "made".into() };
        store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place, content: None, is_name_ignored: false, replaced: None })).unwrap();
        std::fs::write(&in_the_way, b"x").unwrap();
    });
    assert_eq!(operations.make("made", MadeKind::Directory).map(|made| made.item.local_item_id), Err(Errno::EIO));
    assert_eq!(operations.take_downs(), [TakeDownCause::UndoFailed { path: "made".into() }]);
    assert!(!operations.fixture.access_mode_switch.is_read_write(), "the folder takes no further change");
    assert!(matches!(operations.list_records()[..], [PlaceChangeRecord::MakeEntry { .. }]), "the record stands for the check at start: {:?}", operations.list_records());
    operations.clear_hook();
    assert_eq!(operations.make("other", MadeKind::Directory).map(|made| made.item.local_item_id), Err(Errno::EROFS));
}

#[test]
fn every_operation_wakes_the_plan_after_its_row_and_never_before() {
    let operations = OperationsFixture::new();
    operations.fixture.make_own_file("old.txt", b"old");
    operations.take_wake();
    // At the point where the disk is changed and the row is not written, nothing woke
    // the plan; after the operation, something did.
    let woken_before_row = Arc::new(AtomicBool::new(false));
    let (runtime, wake, woken) = (operations.fixture.runtime(), Arc::clone(&operations.fixture.sync_plan_wake), Arc::clone(&woken_before_row));
    operations.run_at(OperationPoint::DiskChanged, move || {
        if take_wake(&runtime, &wake) {
            woken.store(true, Ordering::SeqCst);
        }
    });
    let made = operations.make("new.txt", MadeKind::FileOpenedForWriting).unwrap();
    assert!(operations.take_wake(), "a make wakes the plan");
    operations.rename("old.txt", "renamed.txt").unwrap();
    assert!(operations.take_wake(), "a rename wakes the plan");
    operations.remove("renamed.txt", ItemKind::File).unwrap();
    assert!(operations.take_wake(), "a removal wakes the plan");
    assert!(!woken_before_row.load(Ordering::SeqCst), "no operation woke the plan before its row");
    operations.clear_hook();
    operations.close(made.fh);
    assert!(operations.take_wake(), "the last close wakes the plan");
    let id = made.item.local_item_id;
    let opened = operations.run(move |operations| operations.open_to_write(id)).unwrap();
    assert!(operations.take_wake(), "the first writer's open wakes the plan");
    operations.close(opened.fh);
    operations.take_wake();
    operations.run(move |operations| operations.truncate(id, None, 4)).unwrap();
    assert!(operations.take_wake(), "a truncation wakes the plan");
    operations.run(move |operations| operations.set_modified_time(id, None, 1_650_000_000, 0)).unwrap();
    assert!(operations.take_wake(), "a time set wakes the plan");
}

#[test]
fn a_read_only_folder_refuses_every_operation_before_anything_is_read() {
    let operations = OperationsFixture::new();
    let file = operations.fixture.make_own_file("file.txt", b"content").local_item_id;
    operations.fixture.access_mode_switch.switch_to_read_only();
    // Also for what is not there: nothing was read to say so.
    let nowhere = LocalItemId::new(999);
    assert_eq!(operations.make("new", MadeKind::Directory).map(|made| made.fh), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.make(nowhere, OsStr::new("new"), MadeKind::File)).map(|made| made.fh), Err(Errno::EROFS));
    assert_eq!(operations.rename("file.txt", "other.txt"), Err(Errno::EROFS));
    assert_eq!(operations.remove("file.txt", ItemKind::File), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.remove(nowhere, OsStr::new("x"), ItemKind::File)), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.open_to_write(file)).map(|opened| opened.fh), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.open_to_overwrite(nowhere)).map(|opened| opened.fh), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.truncate(file, None, 0)), Err(Errno::EROFS));
    assert_eq!(operations.run(move |operations| operations.set_modified_time(file, None, 1, 0)), Err(Errno::EROFS));
    let operations_on_mount_thread = Arc::clone(&operations.operations);
    assert_eq!(operations.fixture.run_on_mount_like_thread(move || operations_on_mount_thread.find_file_to_fill(nowhere, None)), Err(Errno::EROFS));
    assert_eq!(operations.list_records(), []);
    assert_eq!(operations.fixture.read_disk_file("file.txt").as_deref(), Some(&b"content"[..]));
    operations.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_reader_that_opens_by_path_during_an_operation_gets_the_file_of_its_row() {
    let operations = OperationsFixture::new();
    // The review's M5: X moves from f to g, and a reader of X that read its row before
    // must get X's file, whichever name it has by the time it opens.
    let moved = operations.fixture.make_own_file("f", b"the file of X").local_item_id;
    let reader: Arc<Mutex<Option<std::thread::JoinHandle<String>>>> = Arc::default();
    let (place_changes, read_connections, started) = (Arc::clone(&operations.fixture.place_changes), Arc::clone(&operations.fixture.read_connections), Arc::clone(&reader));
    operations.run_at(OperationPoint::DiskChanged, move || {
        let (place_changes, read_connections) = (Arc::clone(&place_changes), Arc::clone(&read_connections));
        let thread = std::thread::spawn(move || {
            let opened = place_changes.open_at_row_path(OpenMode::Read, || read_connections.read(|reader| Ok(reader.find_path(moved)?.map(|path| (path, ())))));
            let mut content = String::new();
            opened.unwrap().expect("the row has a path").0.read_to_string(&mut content).unwrap();
            content
        });
        *crate::panic::lock(&started) = Some(thread);
        // The reader gets time to begin while the change is under way: it then sleeps
        // until the change ended. Whenever it begins, what it gets is the same.
        std::thread::sleep(Duration::from_millis(30));
    });
    operations.rename("f", "g").unwrap();
    let read = crate::panic::lock(&reader).take().expect("the reader was started").join().unwrap();
    assert_eq!(read, "the file of X");
}

/// One operation a death is tried in at each of its points.
struct DeathCase {
    name: &'static str,
    prepare: fn(&OperationsFixture),
    run: fn(&OperationsFixture),
    /// Whether the folder shows what the operation was to leave, after the check at
    /// start settled it.
    is_done: fn(&OperationsFixture) -> bool,
    /// At which points the operation is done once the check settled its death.
    done_at: &'static [OperationPoint],
}

fn shows(operations: &OperationsFixture, path: &str) -> bool {
    operations.fixture.find_item_at(path).is_some()
}

fn is_on_disk(operations: &OperationsFixture, path: &str) -> bool {
    operations.fixture.find_item_at(path).is_some_and(|row| row.content_presence == ContentPresence::OnDisk) && operations.fixture.disk_path(path).is_file()
}

const DEATH_CASES: &[DeathCase] = &[
    // A made entry whose row was not written is finished from its record: the entry is there.
    DeathCase { name: "mkdir", prepare: |_| {}, run: |operations| drop(operations.make("d", MadeKind::Directory)), is_done: |operations| shows(operations, "d") && operations.fixture.disk_path("d").is_dir(), done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten] },
    DeathCase { name: "mknod", prepare: |_| {}, run: |operations| drop(operations.make("f.txt", MadeKind::File)), is_done: |operations| is_on_disk(operations, "f.txt"), done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten] },
    DeathCase { name: "create", prepare: |_| {}, run: |operations| drop(operations.make("f.txt", MadeKind::FileOpenedForWriting)), is_done: |operations| is_on_disk(operations, "f.txt"), done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten] },
    DeathCase {
        name: "a file made where one was removed",
        prepare: |operations| {
            let copy = operations.fixture.show_onedrive_file("f.txt", b"as it was").local_item_id;
            operations.fixture.land_copy(copy, b"as it was");
            operations.remove("f.txt", ItemKind::File).unwrap();
        },
        run: |operations| drop(operations.make("f.txt", MadeKind::File)),
        is_done: |operations| operations.fixture.find_item_at("f.txt").is_some_and(|row| row.onedrive_item == Some(ItemFixture::onedrive_reference_of("f.txt"))),
        done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten],
    },
    // A moved or set-aside entry whose row was not written goes back: it did not happen.
    DeathCase {
        name: "rename",
        prepare: |operations| drop(operations.fixture.make_own_file("a.txt", b"a")),
        run: |operations| operations.rename("a.txt", "b.txt").unwrap_or(()),
        is_done: |operations| is_on_disk(operations, "b.txt") && !shows(operations, "a.txt") && !operations.fixture.disk_path("a.txt").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rename over a file on disk",
        prepare: |operations| {
            let copy = operations.fixture.show_onedrive_file("doc.txt", b"as it was").local_item_id;
            operations.fixture.land_copy(copy, b"as it was");
            operations.fixture.make_own_file("doc.new", b"saved");
        },
        run: |operations| operations.rename("doc.new", "doc.txt").unwrap_or(()),
        is_done: |operations| !shows(operations, "doc.new") && operations.fixture.read_disk_file("doc.txt").as_deref() == Some(b"saved") && operations.fixture.list_items().iter().filter(|row| row.kind == ItemKind::File).count() == 1,
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rename over a file not on disk",
        prepare: |operations| {
            operations.fixture.show_onedrive_file("doc.txt", b"in OneDrive");
            operations.fixture.make_own_file("doc.new", b"saved");
        },
        run: |operations| operations.rename("doc.new", "doc.txt").unwrap_or(()),
        is_done: |operations| !shows(operations, "doc.new") && operations.fixture.read_disk_file("doc.txt").as_deref() == Some(b"saved"),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "unlink",
        prepare: |operations| drop(operations.fixture.make_own_file("a.txt", b"a")),
        run: |operations| operations.remove("a.txt", ItemKind::File).unwrap_or(()),
        is_done: |operations| !shows(operations, "a.txt") && !operations.fixture.disk_path("a.txt").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rmdir",
        prepare: |operations| drop(operations.fixture.make_own_directory("d")),
        run: |operations| operations.remove("d", ItemKind::Directory).unwrap_or(()),
        is_done: |operations| !shows(operations, "d") && !operations.fixture.disk_path("d").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    // A directory of OneDrive with no directory on disk: on record all the same, with
    // nothing set aside.
    DeathCase {
        name: "rmdir of a directory that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_directory("docs")),
        run: |operations| operations.remove("docs", ItemKind::Directory).unwrap_or(()),
        is_done: |operations| !shows(operations, "docs"),
        done_at: &[OperationPoint::RowWritten],
    },
    // A backing file made for a row: the row comes first, and a writer's open whose file
    // was never made is put back as it was before the open.
    DeathCase {
        name: "the open for writing of an empty file that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_file("empty.txt", b"")),
        run: |operations| {
            let id = operations.id("empty.txt");
            let _ = operations.run(move |operations| operations.open_to_write(id).map(|opened| opened.fh));
        },
        is_done: |operations| is_on_disk(operations, "empty.txt"),
        done_at: &[OperationPoint::DiskChanged],
    },
    DeathCase {
        name: "the open to overwrite of a file that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive")),
        run: |operations| {
            let id = operations.id("remote.txt");
            let _ = operations.run(move |operations| operations.open_to_overwrite(id).map(|opened| opened.fh));
        },
        is_done: |operations| is_on_disk(operations, "remote.txt"),
        done_at: &[OperationPoint::DiskChanged],
    },
    DeathCase {
        name: "the truncation to nothing of a file that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive")),
        run: |operations| {
            let id = operations.id("remote.txt");
            operations.run(move |operations| operations.truncate(id, None, 0)).unwrap_or(());
        },
        is_done: |operations| is_on_disk(operations, "remote.txt"),
        done_at: &[OperationPoint::DiskChanged],
    },
    // The review's S6: the cases it ran by hand.
    // Nothing of the moved file is on disk: the displaced entry is set aside by itself.
    DeathCase {
        name: "rename of a file not on disk over a file on disk",
        prepare: |operations| {
            operations.fixture.show_onedrive_file("remote.txt", b"in OneDrive");
            operations.fixture.make_own_file("doc.txt", b"here");
        },
        run: |operations| operations.rename("remote.txt", "doc.txt").unwrap_or(()),
        is_done: |operations| !shows(operations, "remote.txt") && !operations.fixture.disk_path("doc.txt").exists() && operations.fixture.find_item_at("doc.txt").is_some_and(|row| row.content_presence == ContentPresence::NotOnDisk),
        done_at: &[OperationPoint::RowWritten],
    },
    // The directory stays on disk with its mark taken off; cut before the row, the check
    // puts the mark back. What no row said in it is a new item after any start.
    DeathCase {
        name: "rmdir of a marked directory that holds what no row says",
        prepare: |operations| {
            let docs = operations.fixture.show_onedrive_directory("docs").local_item_id;
            operations.fixture.make_directory_on_disk(docs);
            operations.fixture.write_disk_file("docs/leftover.txt", b"what no row says");
        },
        run: |operations| operations.remove("docs", ItemKind::Directory).unwrap_or(()),
        is_done: |operations| {
            let is_removed = operations.fixture.list_items().iter().any(|row| row.onedrive_item == Some(ItemFixture::onedrive_reference_of("docs")) && row.local.is_none());
            if !is_removed {
                assert_eq!(operations.fixture.read_disk_marks("docs").onedrive_item_id, mark_of("docs"), "while its row shows it, the directory carries its mark");
            }
            is_removed
        },
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "mkdir in a directory that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_directory("docs")),
        run: |operations| drop(operations.make("docs/d", MadeKind::Directory)),
        is_done: |operations| shows(operations, "docs/d") && operations.fixture.disk_path("docs/d").is_dir(),
        done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten],
    },
    DeathCase {
        name: "create in a directory that is not on disk",
        prepare: |operations| drop(operations.fixture.show_onedrive_directory("docs")),
        run: |operations| drop(operations.make("docs/f.txt", MadeKind::FileOpenedForWriting)),
        is_done: |operations| is_on_disk(operations, "docs/f.txt"),
        done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rename into a directory that is not on disk",
        prepare: |operations| {
            operations.fixture.show_onedrive_directory("docs");
            operations.fixture.make_own_file("a.txt", b"a");
        },
        run: |operations| operations.rename("a.txt", "docs/a.txt").unwrap_or(()),
        is_done: |operations| is_on_disk(operations, "docs/a.txt") && !shows(operations, "a.txt") && !operations.fixture.disk_path("a.txt").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rename of a directory with a file in it",
        prepare: |operations| {
            operations.fixture.make_own_directory("d");
            operations.fixture.make_own_file("d/f.txt", b"f");
        },
        run: |operations| operations.rename("d", "e").unwrap_or(()),
        is_done: |operations| is_on_disk(operations, "e/f.txt") && !shows(operations, "d") && !operations.fixture.disk_path("d").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "rename of a directory over an empty directory",
        prepare: |operations| {
            operations.fixture.make_own_directory("d");
            operations.fixture.make_own_file("d/f.txt", b"f");
            operations.fixture.make_own_directory("e");
        },
        run: |operations| operations.rename("d", "e").unwrap_or(()),
        is_done: |operations| is_on_disk(operations, "e/f.txt") && !shows(operations, "d") && !operations.fixture.disk_path("d").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    DeathCase {
        name: "unlink of a clean copy",
        prepare: |operations| {
            let copy = operations.fixture.show_onedrive_file("doc.txt", b"as it was").local_item_id;
            operations.fixture.land_copy(copy, b"as it was");
        },
        run: |operations| operations.remove("doc.txt", ItemKind::File).unwrap_or(()),
        is_done: |operations| !shows(operations, "doc.txt") && !operations.fixture.disk_path("doc.txt").exists(),
        done_at: &[OperationPoint::RowWritten],
    },
    // The directory a removal left on disk is there before the make: with its record or
    // without, the check finds a directory no row has and makes its item.
    DeathCase {
        name: "mkdir where a directory was left on disk",
        prepare: |operations| {
            let docs = operations.fixture.make_own_directory("docs").local_item_id;
            operations.fixture.write_disk_file("docs/leftover.txt", b"what no row says");
            operations.remove("docs", ItemKind::Directory).unwrap();
            assert!(operations.fixture.find_item(docs).is_none() && operations.fixture.disk_path("docs").is_dir());
        },
        run: |operations| drop(operations.make("docs", MadeKind::Directory)),
        is_done: |operations| shows(operations, "docs") && operations.fixture.disk_path("docs").is_dir(),
        done_at: &POINTS,
    },
    DeathCase {
        name: "mknod of a name that is not UTF-8",
        prepare: |_| {},
        run: |operations| {
            let _ = operations.run(|operations| operations.make(LocalItemId::ROOT, OsStr::from_bytes(NOT_UTF8_NAME), MadeKind::File).map(|made| made.fh));
        },
        is_done: |operations| {
            let row = operations.fixture.read_rows(|reader| reader.find_item_at(LocalItemId::ROOT, OsStr::from_bytes(NOT_UTF8_NAME)));
            row.is_some_and(|row| row.content_presence == ContentPresence::OnDisk) && operations.fixture.folder().join(OsStr::from_bytes(NOT_UTF8_NAME)).is_file()
        },
        done_at: &[OperationPoint::DiskChanged, OperationPoint::RowWritten],
    },
];

/// A name a program may give that is no text.
const NOT_UTF8_NAME: &[u8] = b"caf\xe9.txt";

#[test]
fn a_death_at_each_point_of_each_operation_is_settled_by_the_check_at_start() {
    for case in DEATH_CASES {
        for point in POINTS {
            let operations = OperationsFixture::new();
            (case.prepare)(&operations);
            let bound_before: Vec<_> = operations.fixture.list_items().into_iter().filter_map(|row| row.onedrive_item).collect();
            operations.die_at(point);
            assert!(operations.run_until_death(case.run), "{}: the operation reaches {point:?}", case.name);
            operations.fixture.run_start_check();
            assert_eq!(operations.list_records(), [], "{} cut at {point:?}: the check left no record", case.name);
            operations.fixture.assert_rows_and_disk_agree();
            assert_eq!((case.is_done)(&operations), case.done_at.contains(&point), "{} cut at {point:?}: whether it happened", case.name);
            // Whatever was cut, no item of OneDrive lost its row.
            let bound_after: Vec<_> = operations.fixture.list_items().into_iter().filter_map(|row| row.onedrive_item).collect();
            assert_eq!(bound_after, bound_before, "{} cut at {point:?}: the bindings", case.name);
            // The check is done with it: a second one changes nothing.
            let settled = operations.fixture.list_items();
            operations.fixture.run_start_check();
            assert_eq!(operations.fixture.list_items(), settled, "{} cut at {point:?}: a second check", case.name);
        }
    }
}

/// The id OneDrive has for the item the fixture lists at `path`, as a mark says it.
pub(super) fn mark_of(path: &str) -> Option<String> {
    Some(onedrive_id_of(path))
}
