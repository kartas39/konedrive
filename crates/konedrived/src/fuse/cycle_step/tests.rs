//! The decisions of the step are tested with their modules; here is the step itself, on the
//! parts of a mount built over a folder that no mount lies over.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::reconcile::Deferrals;
use konedrive_tree::{Change, Kind, NewTree, Row};

use super::uncommitted_cycle::{list_removed_ids, UncommittedCycle};
use super::{CycleOutcome, OneDriveChange};
use crate::fuse::conflict_copies::ConflictCopy;
use crate::fuse::directory_contents::tests::row;
use crate::fuse::inotify_bridge::Announcement;
use crate::fuse::mount::tests::{build_mount, build_mount_with, BuiltMount};
use crate::fuse::node_table::NodeId;
use crate::local::held::Rw;

/// Stages `changes` as a cycle of the delta listing in the store of `built`, and has the
/// step applied to it inside that call of the store, as a cycle does; `rw` is what the
/// outbox holds. The cycle is not committed.
fn apply_uncommitted(built: &BuiltMount, changes: Vec<Change>, rw: Option<Rw>) -> CycleOutcome {
    let cycle_step = Arc::clone(built.mount.cycle_step());
    let applied = built.store.call_blocking(move |store| {
        store.begin_staging(NewTree::Delta)?;
        store.stage(&changes)?;
        cycle_step.apply_staged_cycle(store, rw.as_ref())
    });
    applied.unwrap()
}

/// Commits the cycle the step of `built` was last applied to, as a cycle's commit does: a
/// read-write folder's keeps the row of what the outbox holds (`rw`) and lets its change
/// wait. The rows of `items` follow in that commit.
fn commit(built: &BuiltMount, rw: Option<Rw>) {
    let copies_on_disk = built.mount.cycle_step().list_copies_on_disk();
    let committed = built.store.call_blocking(move |store| match rw {
        None => store.commit_staging("link", &copies_on_disk),
        Some(rw) => {
            let held: Vec<String> = store.changed_ids()?.into_iter().filter(|id| rw.held.contains(id) && !rw.removing.contains(id)).collect();
            store.commit_staging_deferring("link", &Deferrals { consumed: &[], whole: &held, content: &[], fetched_at: 0, waits: &[], copies_on_disk: &copies_on_disk })
        }
    });
    committed.unwrap();
}

/// A whole cycle over `built`: `changes` staged, the step applied and the cycle committed.
/// The rows of `items` then say what the mount derives
/// ([`BuiltMount::check_rows_agree`]).
fn apply(built: &BuiltMount, changes: Vec<Change>, rw: Option<Rw>) -> CycleOutcome {
    apply_except(built, changes, rw, &[])
}

/// [`apply`], with the agreement check leaving out the paths `except`, where the test says
/// why the rows and the mount's derivation cannot agree.
fn apply_except(built: &BuiltMount, changes: Vec<Change>, rw: Option<Rw>, except: &[&str]) -> CycleOutcome {
    let outcome = apply_uncommitted(built, changes, rw.clone());
    commit(built, rw);
    built.check_rows_agree_except(except);
    outcome
}

/// The OneDrive file `id` as the fixture's drive has it in `parent`, with another content.
fn with_new_content(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(Row { ctag: Some("another".into()), size: 7, ..row(id, Some(parent), name, Kind::File) })
}

fn rows_said(built: &BuiltMount) -> Vec<(OutboxKind, PathBuf)> {
    built.store.call_blocking(|store| store.outbox_rows()).unwrap().into_iter().map(|row| (row.kind, row.rel)).collect()
}

/// The OneDrive id `A` in the directory `D`, placed at `rel`.
pub(super) fn placed(kind: Kind, rel: &str) -> Option<(Row, PathBuf)> {
    let rel = PathBuf::from(rel);
    let name = rel.file_name().unwrap().to_string_lossy().into_owned();
    Some((row("A", Some("D"), &name, kind), rel))
}

#[test]
fn the_ids_a_cycle_removed_are_those_that_vanished() {
    let change = |id: &str, old, new| OneDriveChange::of(id.to_owned(), old, new).unwrap();
    let changes = [
        change("gone", placed(Kind::File, "a.txt"), None),
        change("new", None, placed(Kind::File, "b.txt")),
        change("moved", placed(Kind::File, "c.txt"), placed(Kind::File, "d.txt")),
        change("stayed", placed(Kind::File, "e.txt"), placed(Kind::File, "e.txt")),
        change("gone too", placed(Kind::Folder, "docs"), None),
    ];
    assert_eq!(list_removed_ids(&changes), ["gone", "gone too"]);
}

#[test]
fn a_downloaded_copy_whose_content_is_stale_is_removed() {
    let built = build_mount();
    built.download("a.txt", "A", b"old");
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Stayed { onedrive_id, .. }] if onedrive_id == "A"));
    assert!(!built.disk.join("a.txt").exists());
}

/// The directories down to the new path are made, each marked with its OneDrive id.
#[test]
fn a_downloaded_copy_moves_with_its_file() {
    let built = build_mount();
    built.download("a.txt", "A", b"content");
    let outcome = apply(&built, vec![Change::Upsert(row("A", Some("D"), "a.txt", Kind::File))], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Moved { onedrive_id, .. }] if onedrive_id == "A"));
    assert!(!built.disk.join("a.txt").exists());
    assert_eq!(std::fs::read(built.disk.join("docs/a.txt")).unwrap(), b"content");
    assert_eq!(xattr::get(built.disk.join("docs"), XATTR_ITEM_ID).unwrap().as_deref(), Some(b"D".as_slice()));
}

/// A file a change of the user's waits on is left as it is: its change from OneDrive waits.
#[test]
fn what_the_outbox_holds_is_skipped() {
    let built = build_mount();
    built.download("a.txt", "A", b"mine");
    let rw = Rw { held: HashSet::from(["A".to_owned()]), ..Rw::default() };
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], Some(rw));
    assert!(outcome.changes.is_empty());
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"mine");
}

/// `docs` goes only once `docs/deep` went, whatever the order the cycle has them in: a
/// directory that still holds one would stay.
#[test]
fn the_directories_gone_from_onedrive_are_removed_deepest_first() {
    let built = build_mount_with(&[Change::Upsert(row("E", Some("D"), "deep", Kind::Folder))], &[]);
    built.make_marked_directory("docs", "D");
    built.make_marked_directory("docs/deep", "E");
    let outcome = apply(&built, vec![Change::Delete("D".into()), Change::Delete("C".into()), Change::Delete("E".into())], None);
    assert!(!built.disk.join("docs/deep").exists() && !built.disk.join("docs").exists());
    assert!(outcome.conflicts.kept_directories.is_empty());
}

/// What the user made where a file of OneDrive comes is moved aside, and in a read-write
/// folder that is recorded in the outbox inside the cycle's own call of the store.
#[test]
fn what_a_cycle_keeps_of_the_users_own_in_a_read_write_folder_is_recorded() {
    let built = build_mount();
    assert!(built.mount.switch_to_read_write(Arc::new(|| {})));
    built.make_own_file("new.txt", b"mine");
    let outcome = apply(&built, vec![Change::Upsert(row("N", Some("R"), "new.txt", Kind::File))], None);
    assert_eq!(outcome.conflicts.moved_aside, [ConflictCopy { original: "new.txt".into(), copy: "new-fedora.txt".into() }]);
    assert_eq!(std::fs::read(built.disk.join("new-fedora.txt")).unwrap(), b"mine");
    assert_eq!(rows_said(&built), [(OutboxKind::Create, PathBuf::from("new-fedora.txt"))]);
}

/// A read-only folder with a row in its outbox would run no further cycle: its cycle moves
/// the user's own aside all the same, and leaves no row. The scan for unrecorded entries
/// records the conflict copy once the folder is switched to read-write.
#[test]
fn what_a_cycle_keeps_of_the_users_own_in_a_read_only_folder_is_recorded_only_once_it_is_read_write() {
    let built = build_mount();
    built.make_own_file("new.txt", b"mine");
    let outcome = apply(&built, vec![Change::Upsert(row("N", Some("R"), "new.txt", Kind::File))], None);
    assert_eq!(outcome.conflicts.moved_aside, [ConflictCopy { original: "new.txt".into(), copy: "new-fedora.txt".into() }]);
    assert_eq!(std::fs::read(built.disk.join("new-fedora.txt")).unwrap(), b"mine");
    assert!(outcome.conflicts.detections.is_empty() && outcome.conflicts.ops.is_empty());
    assert!(rows_said(&built).is_empty());
    assert!(built.mount.switch_to_read_write(Arc::new(|| {})));
    // The scan runs on the write-operation thread: the next job there is after it.
    built.run_on_write_operation_thread(|| {});
    assert_eq!(rows_said(&built), [(OutboxKind::Create, PathBuf::from("new-fedora.txt"))]);
}

/// What a cycle leaves is kept until it is committed, and taken once: the pin of what is
/// gone comes off, and nothing is left for a second commit.
#[test]
fn what_a_cycle_leaves_is_done_once_when_it_is_committed() {
    let built = build_mount_with(&[], &["A"]);
    apply(&built, vec![Change::Delete("A".into())], None);
    let cycle_step = built.mount.cycle_step();
    let vanished = Announcement::NameVanished { path: "a.txt".into(), kind: Kind::File, onedrive_id: "A".into(), parent_id: Some("R".into()) };
    assert_eq!(*crate::panic::lock(&cycle_step.uncommitted_cycle), UncommittedCycle { announcements: vec![vanished], removed_ids: vec!["A".to_owned()], copies_on_disk: Vec::new() });
    assert_eq!(built.mount.files_interface().count_pins(), 1, "not before the cycle is committed");
    cycle_step.announce_committed_cycle();
    assert_eq!(built.mount.files_interface().count_pins(), 0);
    assert_eq!(*crate::panic::lock(&cycle_step.uncommitted_cycle), UncommittedCycle::default());
}

/// A cycle that was applied and never committed is applied again: what it left is not
/// announced twice.
#[test]
fn only_the_last_cycle_applied_leaves_anything() {
    let built = build_mount();
    apply(&built, vec![Change::Delete("A".into())], None);
    apply(&built, vec![Change::Delete("B".into())], None);
    let uncommitted_cycle = crate::panic::lock(&built.mount.cycle_step().uncommitted_cycle);
    assert_eq!(uncommitted_cycle.removed_ids, ["B"]);
    assert!(matches!(&uncommitted_cycle.announcements[..], [Announcement::NameVanished { onedrive_id, .. }] if onedrive_id == "B"));
}

/// Every conflict copy's name of the user's own is taken, so it could not be moved aside:
/// the downloaded copy that moves with its file is not moved onto it, and stays where it
/// was.
#[test]
fn nothing_is_moved_onto_what_is_still_in_the_way() {
    let built = build_mount();
    built.download("a.txt", "A", b"content");
    built.make_marked_directory("docs", "D");
    built.make_own_file("docs/a.txt", b"mine");
    for n in 1..=100 {
        built.make_own_file(&format!("docs/{}", crate::local::names::copy_name("a.txt", "fedora", n)), b"an earlier copy");
    }
    // The rows keep the user's own at the name, and the file of OneDrive where its copy
    // still is; the mount's derivation gives the name to OneDrive's and hides the user's.
    let outcome = apply_except(&built, vec![Change::Upsert(row("A", Some("D"), "a.txt", Kind::File))], None, &["a.txt", "docs/a.txt"]);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Moved { onedrive_id, .. }] if onedrive_id == "A"));
    assert!(outcome.conflicts.moved_aside.is_empty());
    assert_eq!(std::fs::read(built.disk.join("docs/a.txt")).unwrap(), b"mine");
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"content");
}

/// What the backing directory holds of a directory is the directory itself: it moves with
/// all it holds, the user's own too.
#[test]
fn a_directory_of_the_backing_directory_moves_with_its_directory_of_onedrive() {
    let built = build_mount();
    built.make_marked_directory("docs", "D");
    built.make_own_file("docs/own.txt", b"mine");
    let outcome = apply(&built, vec![Change::Upsert(row("D", Some("R"), "papers", Kind::Folder))], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Moved { onedrive_id, .. }] if onedrive_id == "D"));
    assert!(!built.disk.join("docs").exists());
    assert_eq!(std::fs::read(built.disk.join("papers/own.txt")).unwrap(), b"mine");
    assert_eq!(xattr::get(built.disk.join("papers"), XATTR_ITEM_ID).unwrap().as_deref(), Some(b"D".as_slice()));
}

/// A directory at the old path that is marked with another OneDrive id backs that other
/// directory, and is not moved.
#[test]
fn a_directory_at_the_old_path_that_backs_another_directory_is_not_moved() {
    let built = build_mount();
    built.make_marked_directory("docs", "OTHER");
    // A directory marked with an id the store does not have is the user's own to the mount's
    // derivation once no item of OneDrive has its name. No operation made it, so no row says
    // so: the check of the backing directory at start will find such a directory.
    apply_except(&built, vec![Change::Upsert(row("D", Some("R"), "papers", Kind::Folder))], None, &["docs"]);
    assert!(built.disk.join("docs").is_dir() && !built.disk.join("papers").exists());
}

/// A read-write mount whose `a.txt` is downloaded, with `content`, and held open for writing
/// by a program when the folder is switched to read-only — as a forced switch does, which
/// also drops the outbox's rows: nothing holds the file for a cycle any more. With the node
/// id the program holds the file by.
fn build_mount_with_file_open_for_writing(content: &[u8]) -> (BuiltMount, NodeId) {
    let built = build_mount();
    assert!(built.mount.switch_to_read_write(Arc::new(|| {})));
    built.download("a.txt", "A", content);
    let node_id = built.node_id_in_root("a.txt");
    let write_operations = Arc::clone(built.kernel_requests.write_operations());
    built.run_on_write_operation_thread(move || {
        write_operations.open_to_write(node_id).unwrap();
    });
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(built.mount.switch_to_read_only()));
    built.store.call_blocking(|store| store.outbox_drop_all()).unwrap();
    (built, node_id)
}

/// The file has another content in OneDrive, and a program still writes its copy: the copy
/// is not unlinked under the writer. It is the user's own, moved aside to a conflict copy's
/// name, and the file is shown as OneDrive has it. The node the program holds the file by
/// goes with the file it writes.
#[test]
fn a_copy_a_program_writes_is_moved_aside_when_its_file_has_new_content() {
    let (built, node_id) = build_mount_with_file_open_for_writing(b"mine");
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], None);
    assert!(matches!(&outcome.changes[..], [OneDriveChange::Stayed { onedrive_id, .. }] if onedrive_id == "A"));
    assert_eq!(outcome.conflicts.moved_aside, [ConflictCopy { original: "a.txt".into(), copy: "a-fedora.txt".into() }]);
    assert!(!built.disk.join("a.txt").exists());
    assert_eq!(std::fs::read(built.disk.join("a-fedora.txt")).unwrap(), b"mine");
    assert!(xattr::get(built.disk.join("a-fedora.txt"), XATTR_ITEM_ID).unwrap().is_none(), "it is the user's own");
    let cycle_step = built.mount.cycle_step();
    let (entry, path) = cycle_step.node_resolver.find_entry(node_id).unwrap();
    assert!(entry.local_only().is_some() && path == PathBuf::from("a-fedora.txt"));
    assert!(cycle_step.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_ne!(built.node_id_in_root("a.txt"), node_id, "the OneDrive file has a node of its own");
}

/// The same for a file that is gone from OneDrive.
#[test]
fn a_copy_a_program_writes_is_moved_aside_when_its_file_is_removed() {
    let (built, _) = build_mount_with_file_open_for_writing(b"mine");
    let outcome = apply(&built, vec![Change::Delete("A".into())], None);
    assert_eq!(outcome.conflicts.moved_aside, [ConflictCopy { original: "a.txt".into(), copy: "a-fedora.txt".into() }]);
    assert_eq!(std::fs::read(built.disk.join("a-fedora.txt")).unwrap(), b"mine");
}

/// A copy no program writes goes with its content, as before.
#[test]
fn a_copy_whose_writer_closed_it_is_removed_when_its_file_has_new_content() {
    let (built, node_id) = build_mount_with_file_open_for_writing(b"mine");
    let cycle_step = built.mount.cycle_step();
    let open = cycle_step.open_handles.add_writer(node_id, Arc::new(tempfile::tempfile().unwrap())).0;
    for fh in 1..=open.0 {
        cycle_step.open_handles.close_file(crate::fuse::open_handles::Fh(fh));
    }
    let outcome = apply(&built, vec![with_new_content("A", "R", "a.txt")], None);
    assert!(outcome.conflicts.moved_aside.is_empty());
    assert!(!built.disk.join("a.txt").exists() && !built.disk.join("a-fedora.txt").exists());
}

/// A directory that is gone from OneDrive and holds something of the user's own stays, with
/// what it holds, as a directory OneDrive does not have: in the rows it gave its binding
/// up, with what the upload rules say of its name. One whose name is ignored is left here
/// with all it holds.
#[test]
fn a_directory_gone_from_onedrive_that_holds_the_users_own_stays_as_the_users_own() {
    let built = build_mount_with(&[Change::Upsert(row("K", Some("R"), "cache", Kind::Folder))], &[]);
    built.make_marked_directory("docs", "D");
    built.make_marked_directory("cache", "K");
    built.download("docs/c.txt", "C", b"content");
    built.make_own_file("docs/own.txt", b"mine");
    built.make_own_file("cache/own.txt", b"mine");
    apply(&built, vec![Change::Delete("C".into()), Change::Delete("D".into()), Change::Delete("K".into())], None);
    assert!(!built.disk.join("docs/c.txt").exists() && built.disk.join("docs/own.txt").exists() && built.disk.join("cache/own.txt").exists());
    let ignored = built.store.call_blocking(|store| {
        let own = store.item_store().find_item_by_path("cache/own.txt".as_ref())?.unwrap();
        store.item_store().is_ignored(own.local_item_id)
    });
    assert!(ignored.unwrap(), "what is in a directory of the user's own under an ignored name is left here");
}

/// A cycle whose commit failed is staged and applied again: what the step wrote to the rows
/// the first time is not written twice, and the commit that goes through leaves the rows
/// saying what the folder shows.
#[test]
fn a_cycle_applied_again_after_a_commit_that_failed_writes_the_rows_once() {
    let built = build_mount();
    built.make_marked_directory("docs", "D");
    built.download("docs/c.txt", "C", b"content");
    built.make_own_file("docs/own.txt", b"mine");
    built.make_own_file("new.txt", b"mine");
    let changes = vec![Change::Upsert(row("N", Some("R"), "new.txt", Kind::File)), Change::Delete("C".into()), Change::Delete("D".into())];
    let first = apply_uncommitted(&built, changes.clone(), None);
    assert_eq!(first.conflicts.moved_aside, [ConflictCopy { original: "new.txt".into(), copy: "new-fedora.txt".into() }]);
    let again = apply(&built, changes, None);
    assert!(again.conflicts.moved_aside.is_empty());
    assert!(built.disk.join("new-fedora.txt").exists() && built.disk.join("docs/own.txt").exists());
}

/// A change from OneDrive that waited while the outbox held its file is staged by the first
/// cycle of the folder once it is read-only, and passes the step: the copy of the content
/// OneDrive had before goes, and the rows say so.
#[test]
fn a_change_that_waited_is_staged_by_a_read_only_cycle_and_reaches_the_folder() {
    let built = build_mount();
    built.download("a.txt", "A", b"old");
    let rw = Rw { held: HashSet::from(["A".to_owned()]), ..Rw::default() };
    apply(&built, vec![with_new_content("A", "R", "a.txt")], Some(rw));
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"old");
    assert_eq!(built.store.call_blocking(|store| store.deferred_ids()).unwrap(), ["A"]);

    let cycle_step = Arc::clone(built.mount.cycle_step());
    let outcome = built.store.call_blocking(move |store| {
        assert!(store.stage_waiting(&[], false)?, "what waits is staged");
        cycle_step.apply_staged_cycle(store, None)
    });
    assert!(matches!(&outcome.unwrap().changes[..], [OneDriveChange::Stayed { onedrive_id, .. }] if onedrive_id == "A"));
    commit(&built, None);
    built.check_rows_agree();
    assert!(!built.disk.join("a.txt").exists());
    assert!(built.store.call_blocking(|store| store.deferred_ids()).unwrap().is_empty());
}

/// A listing over a folder that has copies already: a file whose whole copy is at its place
/// is on disk in its row, also one that is shown only because its directory came, and that
/// no change of the cycle names.
#[test]
fn a_file_that_appears_where_its_whole_copy_is_has_its_content_on_disk() {
    let built = build_mount_with(&[Change::Upsert(row("X", Some("P"), "x.txt", Kind::File))], &[]);
    built.make_marked_directory("papers", "P");
    built.download("papers/x.txt", "X", b"content");
    built.download("n.txt", "N", b"content");
    apply(&built, vec![Change::Upsert(row("P", Some("R"), "papers", Kind::Folder)), Change::Upsert(row("N", Some("R"), "n.txt", Kind::File))], None);
    let states = built.store.call_blocking(|store| {
        let (n, x) = (store.item_store().find_item_by_path("n.txt".as_ref())?, store.item_store().find_item_by_path("papers/x.txt".as_ref())?);
        Ok((n.and_then(|item| item.file_sync_state()), x.and_then(|item| item.file_sync_state())))
    });
    assert_eq!(states.unwrap(), (Some(konedrive_tree::FileSyncState::Synced), Some(konedrive_tree::FileSyncState::Synced)));
}

/// The item of a create in flight: a file made here was uploaded and marked with its id,
/// and the daemon died before the create was committed; a listing then brings the item. Its
/// row does not take the name the file's row holds: it stays not shown, for the create's
/// commit to merge into the file's row. Until then the rows say the file is the user's own,
/// where the mount's derivation takes the marked file for the copy of the listed item.
#[test]
fn the_listed_item_of_a_create_that_is_not_committed_does_not_take_the_files_name() {
    let built = build_mount();
    let made = built.make_own_file("new.txt", b"mine");
    xattr::set(built.disk.join("new.txt"), XATTR_ITEM_ID, b"N").unwrap();
    xattr::set(built.disk.join("new.txt"), konedrive_fs::placeholder::XATTR_STATE, konedrive_fs::placeholder::State::Hydrated.as_str().as_bytes()).unwrap();
    let outcome = apply_except(&built, vec![Change::Upsert(row("N", Some("R"), "new.txt", Kind::File))], None, &["new.txt"]);
    assert!(outcome.conflicts.moved_aside.is_empty());
    let (shown, listed) = built.store.call_blocking(|store| {
        let reference = konedrive_tree::OneDriveItemReference { drive_id: konedrive_tree::DriveId::new("D").unwrap(), item_id: konedrive_tree::OneDriveItemId::new("N") };
        Ok((store.item_store().find_item_by_path("new.txt".as_ref())?.unwrap(), store.item_store().find_bound_item(&reference)?.unwrap()))
    }).unwrap();
    assert_eq!((shown.local_item_id, shown.onedrive_item), (made, None));
    assert_eq!((listed.local, listed.synced_place), (None, None));
}
