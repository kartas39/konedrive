//! What a reconcile does to the folder, in a read-only folder: OneDrive's
//! answer is staged as tree rows and the cycle's own reconcile is run over
//! them (`remote::testing::World`).

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileExt as _, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{read_ctag, read_progress, write_ctag, write_progress, write_stamp, write_state, Progress, State};
use konedrive_tree::Change;

use super::*;
use crate::folder::locks::InodeKey;
use crate::remote::listing::CycleError;
use crate::remote::testing::{id_at, ino, mode, Options, Says, World};

/// A read-only folder: left under its lock as the daemon keeps it, or with
/// the lock taken off after every step, for a test that puts its own files
/// into it.
pub(super) async fn world(locked: bool) -> World {
    World::new(Options { locked, ..Options::default() }).await
}

pub(super) fn row(id: &str, parent: &str, name: &str, kind: Kind, size: u64) -> Row {
    Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind, size, mtime: 1_700_000_000, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed }
}

pub(super) fn root_row() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

pub(super) fn up(row: Row) -> Change {
    Change::Upsert(row)
}

pub(super) fn folder(id: &str, parent: &str, name: &str) -> Change {
    up(row(id, parent, name, Kind::Folder, 0))
}

pub(super) fn file(id: &str, parent: &str, name: &str) -> Change {
    up(row(id, parent, name, Kind::File, 4096))
}

/// `docs/f.txt`, `docs/deep/g.txt`, `top.bin`.
pub(super) fn tree() -> Vec<Change> {
    vec![root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt"), folder("E", "D", "deep"), file("G", "E", "g.txt"), file("T", "R", "top.bin")]
}

/// A new version of a file: other content, a later time.
fn changed(id: &str, parent: &str, name: &str, size: u64, ctag: &str) -> Change {
    let mut r = row(id, parent, name, Kind::File, size);
    r.ctag = Some(ctag.into());
    r.mtime = 1_700_000_500;
    up(r)
}

/// A name this folder cannot hold: the item is skipped.
fn too_long(id: &str, parent: &str, name: &str, kind: Kind) -> Change {
    up(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::NameTooLong), ..row(id, parent, name, kind, 0) })
}

/// A delta of `changes`, reconciled and committed: what it did.
async fn delta(w: &World, changes: &[Change]) -> Applied {
    w.changed(changes).await.unwrap().applied
}

/// Downloads a file the way a finished fill leaves it: content, cTag, stamp.
pub(super) fn hydrate_by_hand(path: &Path, content: &[u8], ctag: &str) {
    let file = konedrive_fs::placeholder::reopen_writable(&File::open(path).unwrap()).unwrap();
    file.set_len(0).unwrap();
    file.write_all_at(content, 0).unwrap();
    write_ctag(&file, ctag).unwrap();
    write_state(&file, State::Hydrated).unwrap();
    write_stamp(&file).unwrap();
}

async fn handle(w: &World, id: &str) -> Option<konedrive_fs::handle::FileHandle> {
    let id = id.to_owned();
    w.store.call(move |s| s.local_handle(&id)).await.unwrap()
}

/// Issue #104, decisions 4 and 5, read-only: what the reconcile takes
/// off the disk — here a folder that is no longer placed (a name too
/// long) — is forgotten in the store before it goes, and a download into
/// a file in it stops; placed again, it records its new objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_removal_forgets_first_and_stops_a_download() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    assert!(handle(&w, "E").await.is_some() && handle(&w, "G").await.is_some());
    let file = File::open(w.path("docs/deep/g.txt")).unwrap();
    let guard = w.locks.lock(InodeKey::of(&file).unwrap()).await;
    let mut step = w.step(Says::Delta(&[too_long("E", "D", &"x".repeat(300), Kind::Folder)])).await;
    step.apply().await.unwrap();
    assert_eq!((handle(&w, "E").await, handle(&w, "G").await), (None, None), "forgotten before the swap");
    step.commit().await.unwrap();
    assert!(!w.path("docs/deep").exists());
    assert_eq!((handle(&w, "E").await, handle(&w, "G").await), (None, None), "and after it");
    tokio::time::timeout(Duration::from_secs(5), guard.cancelled()).await.expect("the download was told to stop");
    drop(guard);

    delta(&w, &[folder("E", "D", "deep")]).await;
    let placed = konedrive_fs::handle::FileHandle::of(&File::open(w.path("docs/deep/g.txt")).unwrap()).unwrap();
    assert_eq!(handle(&w, "G").await, Some(placed), "placed again, with its new object");
}

/// Review fix 6 of issue #104, read-only: a file of another account being
/// downloaded inside a folder removed in OneDrive is set aside alive, as
/// always — but its download is stopped first and it is a placeholder
/// again, not partly filled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_download_set_aside_for_another_account_is_a_placeholder_again() {
    let claimed: Claimed = std::sync::Arc::new(|id: &str| id == "Y");
    let w = World::new(Options { claimed: Some(claimed), ..Options::default() }).await;
    w.listed_as(&tree()).await;
    let at = w.path("docs/theirs.bin");
    std::fs::write(&at, vec![7u8; 8192]).unwrap();
    let file = File::open(&at).unwrap();
    placeholder::write_item_id(&file, "Y").unwrap();
    placeholder::write_state(&file, State::Hydrating).unwrap();
    let key = InodeKey::of(&file).unwrap();
    drop(file);
    let (held, holding) = tokio::sync::oneshot::channel();
    let fill = tokio::spawn({
        let locks = w.locks.clone();
        async move {
            let guard = locks.lock(key).await;
            held.send(()).unwrap();
            guard.cancelled().await;
        }
    });
    holding.await.unwrap();
    let applied = delta(&w, &[Change::Delete("D".into())]).await;
    fill.await.unwrap();
    let aside = applied.on_disk.rescued.iter().find(|r| r.original.ends_with("theirs.bin")).expect("set aside").rescued.clone();
    let file = File::open(&aside).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly), "a placeholder again");
    assert_eq!(file.metadata().unwrap().blocks(), 0, "with nothing of the stopped download in it");
    assert_eq!(placeholder::read_item_id(&file).unwrap().as_deref(), Some("Y"), "still the other account's");
}

/// a new folder's temporary directory left with
/// no id — killed between `mkdirat` and its label — made every later
/// reconcile fail `EEXIST`, for good. An empty one is cleared and the
/// folder made; one with something in it is rescued first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_folders_temporary_directory_left_without_its_id_is_cleared() {
    let w = world(true).await;
    std::fs::create_dir(w.path(".konedrive-new-D")).unwrap();
    w.listed_as(&[root_row(), folder("D", "R", "docs")]).await;
    assert_eq!(id_at(&w.path("docs")).as_deref(), Some("D"));
    assert!(!w.path(".konedrive-new-D").exists());

    let docs = File::open(w.path("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::create_dir(w.path("docs/.konedrive-new-E"))).unwrap();
    std::fs::write(w.path("docs/.konedrive-new-E/mine.txt"), b"mine").unwrap();
    let applied = delta(&w, &[folder("E", "D", "deep")]).await;
    assert_eq!(id_at(&w.path("docs/deep")).as_deref(), Some("E"));
    assert_eq!(applied.on_disk.rescued.len(), 1, "{:?}", applied.on_disk.rescued);
    assert_eq!(std::fs::read(applied.on_disk.rescued[0].rescued.join("mine.txt")).unwrap(), b"mine");
}

/// a directory of the user's own in the way,
/// holding a placeholder of ours, is rescued with the user's files — and
/// without the placeholder, which stripped of its state read as a file
/// of zeros in the rescue directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rescued_directory_keeps_the_users_files_and_not_our_placeholders() {
    let w = world(false).await;
    w.listed_as(&[root_row()]).await;
    std::fs::create_dir(w.path("docs")).unwrap();
    std::fs::write(w.path("docs/mine.txt"), b"mine").unwrap();
    let docs = File::open(w.path("docs")).unwrap();
    placeholder::create_placeholder(&docs, "cloud.bin", "X", 4096, SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();

    let applied = delta(&w, &[folder("D", "R", "docs")]).await;

    let rescued = &applied.on_disk.rescued[0].rescued;
    assert_eq!(std::fs::read(rescued.join("mine.txt")).unwrap(), b"mine");
    assert!(!rescued.join("cloud.bin").exists(), "a placeholder would read as zeros there");
    assert_eq!(id_at(&w.path("docs")).as_deref(), Some("D"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_changed_in_the_cloud_is_updated_in_place() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    let path = w.path("docs/f.txt");
    let before = ino(&path);
    let applied = delta(&w, &[changed("F", "D", "f.txt", 8192, "c2")]).await;
    assert_eq!(applied.counts.updated, 1);
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!((meta.ino(), meta.len(), meta.mtime()), (before, 8192, 1_700_000_500));
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    assert_eq!(mode(&path), 0o444);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkpoint_of_the_old_version_goes_with_it() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let path = w.path("docs/f.txt");
    {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        file.write_all_at(&[5u8; 2048], 0).unwrap();
        write_progress(&file, &Progress { ctag: "c-F".into(), bytes: 2048 }).unwrap();
    }
    delta(&w, &[changed("F", "D", "f.txt", 4096, "c2")]).await;
    let file = File::open(&path).unwrap();
    assert_eq!(read_progress(&file).unwrap(), None);
    assert!(std::fs::read(&path).unwrap().iter().all(|b| *b == 0), "the old version's bytes are gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_emptied_in_the_cloud_becomes_an_empty_downloaded_file() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    delta(&w, &[changed("F", "D", "f.txt", 0, "c2")]).await;
    let file = File::open(w.path("docs/f.txt")).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert!(stamp_matches(&file).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downloaded_file_of_the_same_version_is_left_alone() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    hydrate_by_hand(&w.path("docs/f.txt"), b"content", "c-F");
    let mut same = row("F", "D", "f.txt", Kind::File, 7);
    same.mtime = 1_700_000_900; // metadata changed, content did not
    let applied = delta(&w, &[up(same)]).await;
    assert!(applied.pending.replacements.is_empty());
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_changed_here_and_in_the_cloud_is_rescued_and_shown_as_the_new_version() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let path = w.path("docs/f.txt");
    hydrate_by_hand(&path, b"content", "c-F");
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all_at(b" and mine", 7).unwrap();
    let applied = delta(&w, &[changed("F", "D", "f.txt", 9, "c2")]).await;
    assert_eq!(applied.on_disk.rescued.len(), 1);
    assert_eq!(applied.on_disk.rescued[0].original, PathBuf::from("docs/f.txt"), "where it was, for the conflict");
    assert_eq!(std::fs::read(&applied.on_disk.rescued[0].rescued).unwrap(), b"content and mine");
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
}

/// An incremental cycle says what it did item by item —
/// added, updated, moved (and from where), removed — and a folder removed
/// with everything in it is one removal, not one per file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_reconcile_says_each_item_it_changed() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let before = w.activity().len();
    delta(&w, &[file("N", "R", "new.txt"), changed("F", "D", "f.txt", 9, "c2"), file("T", "R", "renamed.bin"), Change::Delete("E".into())]).await;
    let mut said = w.activity().split_off(before);
    said.sort_by(|a, b| a.1.cmp(&b.1));
    let event = |kind: &str, rel: &str, detail: String| (kind.to_owned(), w.full(rel), detail);
    assert_eq!(
        said,
        vec![
            event("removed", "docs/deep", String::new()),
            event("updated", "docs/f.txt", String::new()),
            event("added", "new.txt", String::new()),
            event("moved", "renamed.bin", format!("from {}", w.full("top.bin"))),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_being_filled_is_left_for_the_next_cycle() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let path = w.path("docs/f.txt");
    let _held = w.locks.try_lock(InodeKey::of(&File::open(&path).unwrap()).unwrap()).unwrap();
    let applied = delta(&w, &[changed("F", "D", "f.txt", 8192, "c2")]).await;
    assert_eq!((applied.counts.updated, applied.counts.deferred), (0, 1));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_reconcile_builds_the_tree_from_nothing() {
    let w = world(false).await;
    let applied = w.listed_as(&tree()).await;
    assert_eq!(applied.counts.created, 5);
    for (rel, id) in [("docs", "D"), ("docs/f.txt", "F"), ("docs/deep", "E"), ("docs/deep/g.txt", "G"), ("top.bin", "T")] {
        assert_eq!(id_at(&w.path(rel)).as_deref(), Some(id), "{rel}");
    }
    let meta = std::fs::metadata(w.path("docs/f.txt")).unwrap();
    assert_eq!((meta.len(), meta.mtime()), (4096, 1_700_000_000));
    assert!(!w.path(".konedrive-holding").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn under_the_lock_everything_ends_read_only() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    for rel in ["", "docs", "docs/deep"] {
        assert_eq!(mode(&w.path(rel)), 0o555, "{rel:?}");
    }
    for rel in ["docs/f.txt", "top.bin", "docs/deep/g.txt"] {
        assert_eq!(mode(&w.path(rel)), 0o444, "{rel}");
    }
    let refused = std::fs::write(w.path("docs/new.txt"), b"x").unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_keeps_the_inode() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    let before = ino(&w.path("docs/f.txt"));
    delta(&w, &[file("F", "D", "renamed.txt")]).await;
    assert_eq!(ino(&w.path("docs/renamed.txt")), before);
    assert!(!w.path("docs/f.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_folder_takes_its_contents_along() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    let before = ino(&w.path("docs/deep/g.txt"));
    delta(&w, &[folder("E", "R", "moved")]).await;
    assert_eq!(ino(&w.path("moved/g.txt")), before);
    assert_eq!(id_at(&w.path("moved")).as_deref(), Some("E"));
    assert!(!w.path("docs/deep").exists());
    assert_eq!(mode(&w.path("moved")), 0o555);
}

/// Phase 1 goes deepest first: when a folder and something inside it both
/// move, the inner one leaves before the folder changes its path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_and_a_file_inside_it_move_in_one_delta() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    let before = ino(&w.path("docs/deep/g.txt"));
    delta(&w, &[folder("E", "R", "moved"), file("G", "R", "g.txt")]).await;
    assert_eq!(ino(&w.path("g.txt")), before);
    assert_eq!(id_at(&w.path("moved")).as_deref(), Some("E"));
    assert!(!w.path("docs/deep").exists());
}

/// The same order in a Full reconcile, which finds the misplaced items by
/// scanning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_reconcile_moves_the_inner_of_two_misplaced_items_first() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let before = ino(&w.path("docs/deep/g.txt"));
    w.changed_full(&[folder("E", "R", "deep"), file("G", "E", "g2.txt")]).await.unwrap();
    assert_eq!(ino(&w.path("deep/g2.txt")), before);
    assert!(!w.path("docs/deep").exists());
}

/// Names exchanged among siblings in one delta: through the holding
/// directory, which is left empty and removed; nothing is rescued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_names_swapped_end_up_swapped() {
    let w = world(true).await;
    w.listed_as(&[root_row(), file("A", "R", "a"), file("B", "R", "b")]).await;
    let (a, b) = (ino(&w.path("a")), ino(&w.path("b")));
    let applied = delta(&w, &[file("A", "R", "b"), file("B", "R", "a")]).await;
    assert_eq!((ino(&w.path("b")), ino(&w.path("a"))), (a, b));
    assert!(applied.on_disk.rescued.is_empty());
    assert!(!w.path(".konedrive-holding").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_folder_goes_but_a_file_changed_here_is_rescued() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    // f.txt was downloaded, then written to through a descriptor someone
    // opened during a lock window.
    let path = w.path("docs/f.txt");
    {
        let file = konedrive_fs::placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
        file.write_all_at(&[9u8; 4096], 0).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();
        // Appended, so the size changes: an mtime can land in the same
        // clock tick as the stamp.
        file.write_all_at(b"local work", 4096).unwrap();
    }
    let applied = delta(&w, &[Change::Delete("D".into())]).await;
    assert!(!w.path("docs").exists());
    assert_eq!(applied.on_disk.rescued.len(), 1, "{:?}", applied.on_disk.rescued);
    assert_eq!(applied.on_disk.rescued[0].original, PathBuf::from("docs/f.txt"));
    let kept = &applied.on_disk.rescued[0].rescued;
    assert!(kept.starts_with(&w.rescue_dir) && kept.ends_with("docs/f.txt"), "{kept:?}");
    assert!(std::fs::read(kept).unwrap().ends_with(b"local work"));
    assert_eq!(mode(kept), 0o644);
    let names: Vec<_> = xattr::list(kept).unwrap().collect();
    assert!(names.iter().all(|n| !n.to_string_lossy().starts_with("user.konedrive.")), "{names:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_too_long_is_not_created_and_a_folder_renamed_to_one_leaves() {
    let w = world(true).await;
    let long = "я".repeat(128);
    w.listed_as(&[root_row(), too_long("L", "R", &long, Kind::File), folder("D", "R", "docs"), file("F", "D", "f.txt")]).await;
    assert_eq!(std::fs::read_dir(&w.root.path).unwrap().count(), 1, "only docs");
    delta(&w, &[too_long("D", "R", &long, Kind::Folder)]).await;
    assert!(!w.path("docs").exists(), "a folder that can no longer be shown is removed; its clean files are in the cloud");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_order_of_the_rows_does_not_matter() {
    let w = world(false).await;
    let mut changes = tree();
    changes.reverse();
    w.listed_as(&changes).await;
    assert_eq!(id_at(&w.path("docs/deep/g.txt")).as_deref(), Some("G"));
}

/// A Changed reconcile that finds the folder not matching the stored tree
/// hands over to a Full one, which places the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_that_does_not_match_the_stored_tree_is_reconciled_in_full() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    std::fs::remove_file(w.path("docs/f.txt")).unwrap();
    let done = w.changed(&[file("F", "D", "renamed.txt")]).await.unwrap();
    assert!(done.full, "handed over");
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
}

/// A stranger directory where a new folder belongs leaves whole, by one
/// rename, even locked and read-only itself; it arrives as the user's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_folder_in_the_way_is_rescued_whole_under_the_lock() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    std::fs::set_permissions(&w.root.path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(w.path("incoming")).unwrap();
    std::fs::write(w.path("incoming/mine.txt"), b"mine").unwrap();
    std::fs::set_permissions(w.path("incoming"), std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::set_permissions(&w.root.path, std::fs::Permissions::from_mode(0o555)).unwrap();
    let applied = delta(&w, &[folder("N", "R", "incoming")]).await;
    assert_eq!(applied.on_disk.rescued.len(), 1, "{:?}", applied.on_disk.rescued);
    assert_eq!(applied.on_disk.rescued[0].original, PathBuf::from("incoming"));
    let aside = &applied.on_disk.rescued[0].rescued;
    assert_eq!(std::fs::read(aside.join("mine.txt")).unwrap(), b"mine");
    assert_eq!(mode(aside), 0o755);
    assert_eq!(id_at(&w.path("incoming")).as_deref(), Some("N"));
    assert_eq!(mode(&w.root.path), 0o555);
}

/// An item is put only into a folder that is ours: a folder swapped for a
/// stranger of the same name (the lock bypassed) is reconciled in full —
/// the stranger is rescued, the folder comes back by its id, and the new
/// item is made in it, never in the stranger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_item_is_never_placed_into_a_folder_that_is_not_ours() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let ours = ino(&w.path("docs/deep"));
    std::fs::rename(w.path("docs/deep"), w.path("elsewhere")).unwrap();
    std::fs::create_dir(w.path("docs/deep")).unwrap();
    let stranger = ino(&w.path("docs/deep"));
    let done = w.changed(&[file("N", "E", "new.txt")]).await.unwrap();
    assert!(done.full, "handed over");
    assert_eq!(ino(&w.path("docs/deep")), ours, "the folder is back where the tree has it");
    assert_eq!(id_at(&w.path("docs/deep/new.txt")).as_deref(), Some("N"));
    let aside = &done.applied.on_disk.rescued.iter().find(|r| r.original == Path::new("docs/deep")).expect("the stranger is rescued").rescued;
    assert_eq!(ino(aside), stranger);
    assert_eq!(std::fs::read_dir(aside).unwrap().count(), 0, "nothing was made in it");
}

/// A holding directory left by an earlier run, here with a file still in
/// the tree: the next reconcile is a Full one, which puts the file back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holding_directory_left_behind_is_drained_by_a_full_reconcile() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    std::fs::create_dir(w.path(".konedrive-holding")).unwrap();
    std::fs::rename(w.path("top.bin"), w.path(".konedrive-holding/T")).unwrap();
    let done = w.changed(&[file("F", "D", "renamed.txt")]).await.unwrap();
    assert!(done.full, "handed over");
    assert_eq!(id_at(&w.path("top.bin")).as_deref(), Some("T"));
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!w.path(".konedrive-holding").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_reconcile_repairs_whatever_it_finds() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    // A file of ours in the wrong folder, a stranger where a new item
    // belongs, and a folder a crash left under its temporary name.
    std::fs::rename(w.path("docs/f.txt"), w.path("f-in-the-wrong-place")).unwrap();
    std::fs::write(w.path("docs/new.txt"), b"mine").unwrap();
    std::fs::rename(w.path("docs/deep"), w.path("docs/.konedrive-new-E")).unwrap();
    let applied = w.changed_full(&[file("N", "D", "new.txt")]).await.unwrap().applied;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(id_at(&w.path("docs/deep")).as_deref(), Some("E"));
    assert_eq!(id_at(&w.path("docs/new.txt")).as_deref(), Some("N"));
    assert_eq!(applied.on_disk.rescued.len(), 1, "{:?}", applied.on_disk.rescued);
    assert_eq!(applied.on_disk.rescued[0].original, PathBuf::from("docs/new.txt"));
    assert_eq!(std::fs::read(&applied.on_disk.rescued[0].rescued).unwrap(), b"mine");
    assert!(!w.path("f-in-the-wrong-place").exists());
}

/// What `swap_in` leaves when a crash — or a rename
/// that fails after its `linkat` succeeded — lands before the file it
/// downloaded ever lands on the old name: a second name for the same
/// item id, carrying the new version, that a Full reconcile must not try
/// to send to holding alongside the real (still current) file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_link_left_by_a_crashed_swap_is_discarded_and_the_real_file_still_moves() {
    let w = world(false).await;
    w.listed_as(&tree()).await;
    let disk = Disk::open(&w.root, false).unwrap();
    let dir = disk.dir(Path::new("docs")).unwrap();
    let leftover = disk.tmpfile(&dir).unwrap();
    placeholder::write_item_id(&leftover, "F").unwrap();
    leftover.write_all_at(b"the new version", 0).unwrap();
    placeholder::write_ctag(&leftover, "c2").unwrap();
    placeholder::write_state(&leftover, State::Hydrated).unwrap();
    placeholder::write_stamp(&leftover).unwrap();
    nix::unistd::linkat(leftover.as_fd(), "", dir.as_fd(), OsStr::new(".konedrive-new-F"), nix::fcntl::AtFlags::AT_EMPTY_PATH).unwrap();
    drop(leftover);
    assert!(w.path("docs/.konedrive-new-F").exists());

    let applied = w.changed_full(&[file("F", "D", "renamed.txt")]).await.unwrap().applied;
    assert!(!w.path("docs/.konedrive-new-F").exists(), "the leftover is gone");
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!w.path("docs/f.txt").exists());
    assert!(applied.on_disk.rescued.is_empty(), "nothing here held local work");
}

/// Invariant M1: a new folder is marked before anything is created in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_folder_is_marked_while_it_is_still_empty() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    let seen = w.helper.marks();
    assert_eq!(seen.len(), 2, "docs and docs/deep were marked");
    assert!(seen.iter().all(|m| m.entries == 0), "entries at the time of marking: {seen:?}");
    let mut names: Vec<&str> = seen.iter().map(|m| m.name.as_str()).collect();
    names.sort();
    assert_eq!(names, [".konedrive-new-D", ".konedrive-new-E"], "marked before the real name shows the folder");
}

/// Invariant M1 across a failure: a folder whose marking failed is left
/// under its temporary name, and the Full reconcile that later places it
/// marks it before its real name shows it and before anything is put in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_whose_marking_failed_is_marked_when_it_is_placed_later() {
    let w = world(true).await;
    let listing = [root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt")];
    w.helper.refuse_marks(libc::EIO);
    let err = w.step(Says::Whole(&listing)).await.run().await.map(|_| ()).unwrap_err();
    assert!(matches!(err, CycleError::Apply(_)), "{err:?}");
    let unmarked = ino(&w.path(".konedrive-new-D"));
    let refused = w.helper.marks().len();

    w.helper.refuse_marks(0);
    w.listed_as(&listing).await;
    assert_eq!(ino(&w.path("docs")), unmarked, "the folder made the first time is the one placed");
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"));
    let seen = w.helper.marks().split_off(refused);
    assert!(
        seen.iter().any(|m| m.ino == unmarked && m.entries == 0 && m.name != "docs"),
        "docs must be marked while empty, before its real name shows it: {seen:?}"
    );
}

/// The same for the holding directory: one left by a cycle whose marking
/// failed is marked before anything is moved into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holding_directory_whose_marking_failed_is_marked_before_it_is_used() {
    let w = world(true).await;
    w.listed_as(&tree()).await;
    w.helper.refuse_marks(libc::EIO);
    let err = w.changed(&[file("F", "D", "renamed.txt")]).await.map(|_| ()).unwrap_err();
    assert!(matches!(err, CycleError::Apply(_)), "{err:?}");
    let holding = ino(&w.path(".konedrive-holding"));
    let refused = w.helper.marks().len();

    w.helper.refuse_marks(0);
    w.changed_full(&[file("F", "D", "renamed.txt")]).await.unwrap();
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
    let seen = w.helper.marks().split_off(refused);
    assert!(seen.iter().any(|m| m.ino == holding && m.entries == 0), "the holding directory was never marked: {seen:?}");
}

/// An item OneDrive dates before 1970 gets its placeholder like any other:
/// the time the placeholder carries is cut to 1970-01-01 (`file::cloud_time`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_dated_before_1970_gets_a_placeholder_dated_1970() {
    let w = world(false).await;
    let mut old = row("F", "R", "old.txt", Kind::File, 4096);
    old.mtime = -86_400;
    let applied = w.listed_as(&[root_row(), up(old)]).await;
    assert_eq!(applied.counts.created, 1);
    let path = w.path("old.txt");
    assert_eq!(id_at(&path).as_deref(), Some("F"));
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!((meta.len(), meta.mtime()), (4096, 0));
}
