//! Read-write mode's reconcile rules (`docs/design/writes.md` §9, §7): the
//! folder is listed, changed by hand, and reconciled with a delta staged as
//! tree rows, through the cycle's own reconcile and commit
//! (`remote::testing::World`) — what the disk does not show is deferred.

use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID};

use crate::folder::disk::Disk;
use crate::local::IgnoreList;
use crate::remote::listing::CycleError;
use crate::remote::materialize::Applied;
use crate::remote::testing::{edit, id_at, write_version, Options, World};
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::{Change, Kind, Placement, Row, Table};

fn row(id: &str, parent: &str, name: &str, kind: Kind, ctag: &str) -> Row {
    Row {
        id: id.into(),
        parent_id: Some(parent.into()),
        name: name.into(),
        kind,
        size: if kind == Kind::File { 3 } else { 0 },
        mtime: 1_700_000_000,
        etag: Some(format!("e-{id}-{ctag}")),
        ctag: Some(ctag.into()),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Change {
    Change::Upsert(row(id, parent, name, Kind::File, ctag))
}

fn folder(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(row(id, parent, name, Kind::Folder, "c"))
}

/// `docs/f.txt`, `docs/deep/g.txt`, `top.txt`.
fn tree() -> Vec<Change> {
    let root = Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
    vec![
        Change::Root(root),
        folder("D", "R", "docs"),
        file("F", "D", "f.txt", "c1"),
        folder("E", "D", "deep"),
        file("G", "E", "g.txt", "c1"),
        file("T", "R", "top.txt", "c1"),
    ]
}

/// A read-write folder that holds [`tree`], listed once.
async fn listed() -> World {
    let w = World::new(Options { writes: true, ..Options::default() }).await;
    w.listed_as(&tree()).await;
    w
}

/// A delta of `changes` reconciled in read-write mode — over what it
/// changes, or in full — and committed as a cycle commits it.
async fn cycle(w: &World, changes: &[Change], full: bool) -> Result<Applied, CycleError> {
    let done = if full { w.changed_full(changes).await } else { w.changed(changes).await };
    done.map(|done| done.applied)
}

async fn handle(w: &World, id: &str) -> Option<konedrive_fs::handle::FileHandle> {
    let id = id.to_owned();
    w.store.call(move |s| s.local_handle(&id)).await.unwrap()
}

/// §9: an item of ours away from where the base has it — a local move the
/// examination has not seen yet — is left where it is, never made again
/// where the base has it; the delta's change to it waits, and the base keeps
/// the version the file holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_move_not_examined_yet_is_left_where_it_is_and_its_change_waits() {
    let fx = listed().await;
    std::fs::rename(fx.path("docs/f.txt"), fx.path("docs/moved.txt")).unwrap();
    let applied = cycle(&fx, &[file("F", "D", "f.txt", "c2")], true).await.unwrap();
    assert_eq!(id_at(&fx.path("docs/moved.txt")).as_deref(), Some("F"), "left where the user put it");
    assert!(!fx.path("docs/f.txt").exists(), "not made again at its old name");
    assert!(applied.pending.unsettled.contains("F"));
    assert_eq!(fx.base("F").unwrap().ctag.as_deref(), Some("c1"), "the base keeps what the file holds");
    assert_eq!(fx.deferred("F"), Some(file("F", "D", "f.txt", "c2")), "OneDrive's change waits");
}

/// `TR2`: a row dropped for what OneDrive decided forgets its item's local
/// objects by the new tree too — here what a cycle that failed left staged,
/// which moves `top.txt` into the folder. The file is still where it was:
/// the next cycle finds it by its id and moves it, records it again, and
/// places no second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_forgotten_while_its_move_was_staged_is_moved_and_not_placed_twice() {
    use std::os::unix::fs::MetadataExt;
    let w = listed().await;
    let inode = std::fs::metadata(w.path("top.txt")).unwrap().ino();
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    let seq = w.row(OutboxKind::Delete, Some("D"), "docs");
    let moved = [file("T", "D", "top.txt", "c1")];
    w.store
        .call({
            let moved = moved.to_vec();
            move |s| {
                s.begin_staging(konedrive_tree::NewTree::Delta)?;
                s.stage(&moved)?;
                s.outbox_drop(seq, Some("D"), None)
            }
        })
        .await
        .unwrap();
    assert_eq!(handle(&w, "T").await, None, "forgotten: the new tree has it below the folder");

    cycle(&w, &moved, false).await.unwrap();
    assert_eq!(std::fs::metadata(w.path("docs/top.txt")).unwrap().ino(), inode, "the file itself, moved");
    assert!(!w.path("top.txt").exists());
    assert!(w.path("docs/f.txt").exists(), "the folder is placed again");
    let there = konedrive_fs::handle::FileHandle::of(&File::open(w.path("docs/top.txt")).unwrap()).unwrap();
    assert_eq!(handle(&w, "T").await, Some(there), "and recorded again");
}

/// §9: an item with a live outbox row — in any state — is not moved,
/// replaced or removed, nor is anything below a folder a row moves; their
/// changes wait. A disagreement those rows explain never turns a Changed
/// scope Full. Below a folder a `delete` row removes, nothing is placed, and
/// the delta goes to the base (the read-write reconcile must, item 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_keep_the_reconcile_off_their_items_and_a_changed_scope_does_not_turn_full() {
    let fx = listed().await;
    fx.row(OutboxKind::Update, Some("F"), "docs/f.txt");
    std::fs::rename(fx.path("docs/deep"), fx.path("docs/deeper")).unwrap();
    fx.row(OutboxKind::Move, Some("E"), "docs/deeper");
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    fx.row(OutboxKind::Delete, Some("T"), "top.txt");
    let changes = [file("F", "D", "renamed.txt", "c2"), file("N", "E", "n.txt", "c1"), file("T", "R", "top.txt", "c2")];
    let done = fx.changed(&changes).await.unwrap();
    assert!(!done.full, "no Full reconcile over rows");
    let applied = done.applied;
    assert!(fx.path("docs/f.txt").exists() && !fx.path("docs/renamed.txt").exists(), "not moved under its row");
    assert!(!fx.path("docs/deeper/n.txt").exists() && !fx.path("docs/deep").exists(), "nothing placed where a moving folder was");
    assert!(!fx.path("top.txt").exists(), "a delete row's item is not placed again by the reconcile");
    assert_eq!(fx.base("F").unwrap().name, "f.txt");
    assert!(fx.deferred("F").is_some() && fx.deferred("N").is_some(), "{:?}", applied.pending.unsettled);
    assert_eq!(fx.base("T").unwrap().ctag.as_deref(), Some("c2"), "below a removal the delta goes to the base");
    assert!(fx.deferred("T").is_none());
}

/// §9: a tree item missing here is made again only with something to
/// place — new, or with no local object on record (the outbox forgot it once
/// OneDrive's change won, delete × edit, §7). One whose local object is on
/// record and changed in OneDrive is not placed again, in either scope: its
/// object may be alive out of the folder, and the
/// examination decides first, from its base place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_item_is_placed_again_only_with_something_to_place() {
    let fx = listed().await;
    std::fs::remove_file(fx.path("docs/f.txt")).unwrap();
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    for full in [false, true] {
        let applied = cycle(&fx, &[file("T", "R", "top.txt", "c2")], full).await.expect("never a hand-over for it");
        assert!(!fx.path("top.txt").exists(), "full={full}: not placed while its object may be alive");
        assert!(applied.pending.unsettled.contains("T") && fx.deferred("T").is_some(), "its change waits");
        assert!(applied.on_disk.examine.contains(&(PathBuf::from("top.txt"), false)), "the examination decides: {:?}", applied.on_disk.examine);
    }
    assert!(!fx.path("docs/f.txt").exists(), "a delete not examined yet is not undone");

    // The outbox forgets the object once OneDrive's change won: then it comes back.
    fx.store.call(move |s| s.set_local_handle("T", None)).await.unwrap();
    cycle(&fx, &[file("T", "R", "top.txt", "c2")], false).await.unwrap();
    assert_eq!(id_at(&fx.path("top.txt")).as_deref(), Some("T"), "changed in OneDrive: it comes back");
    assert_eq!(placeholder::read_state(&File::open(fx.path("top.txt")).unwrap()).unwrap(), Some(State::OnlineOnly));

    // An item the outbox forgot (its local object dropped) is placed again.
    fx.store.call(move |s| s.set_local_handle("F", None)).await.unwrap();
    cycle(&fx, &[], false).await.unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert!(fx.store.call(move |s| s.local_handle("F")).await.unwrap().is_some(), "and its object recorded again");
}

/// §9, §7 create/create: a file of the user's where a new item arrives is
/// kept beside it, `name-<machine>`, stripped, for the outbox to upload; the
/// item takes the name. A pending `create` there is the outbox's to settle:
/// the item waits. And a save not examined yet at an item OneDrive did not
/// change is the user's, not a conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_file_in_the_way_is_kept_beside_the_clouds_but_a_pending_create_is_left_to_the_outbox() {
    let fx = listed().await;
    std::fs::write(fx.path("docs/new.txt"), b"mine").unwrap();
    let applied = cycle(&fx, &[file("N", "D", "new.txt", "c1")], false).await.unwrap();
    assert_eq!(std::fs::read(fx.path("docs/new-fedora.txt")).unwrap(), b"mine");
    assert_eq!(id_at(&fx.path("docs/new-fedora.txt")), None);
    assert_eq!(id_at(&fx.path("docs/new.txt")).as_deref(), Some("N"));
    assert_eq!(applied.on_disk.copies.len(), 1);
    assert!(applied.on_disk.examine.contains(&(PathBuf::from("docs/new-fedora.txt"), false)));

    std::fs::write(fx.path("docs/other.txt"), b"mine too").unwrap();
    fx.row(OutboxKind::Create, None, "docs/other.txt");
    let applied = cycle(&fx, &[file("O", "D", "other.txt", "c1")], false).await.unwrap();
    assert_eq!(std::fs::read(fx.path("docs/other.txt")).unwrap(), b"mine too", "the outbox settles create/create");
    assert!(applied.on_disk.copies.is_empty() && applied.pending.unsettled.contains("O"));
    assert!(fx.base("O").is_none() && fx.deferred("O").is_some(), "the new item waits");

    // A save by rename (the placeholder replaced by a new file): OneDrive
    // changed nothing, so the file stays as it is.
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    std::fs::write(fx.path("top.txt"), b"saved").unwrap();
    let applied = cycle(&fx, &[], true).await.unwrap();
    assert_eq!(std::fs::read(fx.path("top.txt")).unwrap(), b"saved");
    assert!(applied.on_disk.copies.is_empty());

    // M3: a folder made here where a new one arrives from OneDrive is left
    // for the two to merge (the `mkdir`'s `409`), never copied aside.
    std::fs::create_dir(fx.path("photos")).unwrap();
    std::fs::write(fx.path("photos/mine.jpg"), b"jpg").unwrap();
    let applied = cycle(&fx, &[folder("P", "R", "photos"), file("Q", "P", "theirs.jpg", "c1")], false).await.unwrap();
    assert!(applied.on_disk.copies.is_empty() && !fx.path("photos-fedora").exists());
    assert_eq!(id_at(&fx.path("photos")), None);
    assert!(fx.path("photos/mine.jpg").exists() && !fx.path("photos/theirs.jpg").exists());
    assert!(fx.deferred("P").is_some() && fx.deferred("Q").is_some(), "they wait for the merge");
}

/// §7 edit × edit, found by the reconcile: the changed download is kept as
/// `name-<machine>`, stripped, and OneDrive's version takes the name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_here_and_in_onedrive_keeps_both() {
    let fx = listed().await;
    write_version(&fx.path("docs/f.txt"), b"one", "c1");
    edit(&fx.path("docs/f.txt"), b" and mine");
    let applied = cycle(&fx, &[file("F", "D", "f.txt", "c2")], false).await.unwrap();
    assert_eq!(std::fs::read(fx.path("docs/f-fedora.txt")).unwrap(), b"one and mine");
    assert_eq!(id_at(&fx.path("docs/f-fedora.txt")), None, "the user's own file now");
    assert_eq!(placeholder::read_ctag(&File::open(fx.path("docs/f.txt")).unwrap()).unwrap().as_deref(), Some("c2"));
    assert_eq!(applied.on_disk.copies[0].copy, PathBuf::from("docs/f-fedora.txt"));
    assert_eq!(fx.base("F").unwrap().ctag.as_deref(), Some("c2"));
}

/// What OneDrive removed: what OneDrive had and the daemon placed goes
/// — files not downloaded, a download unchanged since, a folder with
/// nothing left in it. What OneDrive never had stays as the user's own,
/// its attributes off, with the folders above it: a file made here, a
/// download changed here, emptied here, or open for writing, a file from
/// elsewhere holding data, an ignored file with data. A symlink stays beside
/// them. The base takes the removal, and nothing waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_onedrive_removed_keeps_only_what_it_never_had() {
    let fx = listed().await;
    cycle(&fx, &[folder("X", "D", "empty"), file("H", "D", "h.txt", "c1"), file("O", "D", "open.txt", "c1"), file("Z", "D", "zero.txt", "c1")], false).await.unwrap();
    write_version(&fx.path("docs/f.txt"), b"one", "c1");
    edit(&fx.path("docs/f.txt"), b" and mine");
    write_version(&fx.path("docs/h.txt"), b"one", "c1");
    write_version(&fx.path("docs/open.txt"), b"one", "c1");
    let open = std::fs::OpenOptions::new().read(true).write(true).open(fx.path("docs/open.txt")).unwrap();
    write_version(&fx.path("docs/zero.txt"), b"one", "c1");
    std::thread::sleep(std::time::Duration::from_millis(10));
    File::options().write(true).truncate(true).open(fx.path("docs/zero.txt")).unwrap();
    std::fs::write(fx.path("docs/deep/mine.txt"), b"new here").unwrap();
    std::fs::write(fx.path("docs/.~lock.f.txt#"), b"lock").unwrap();
    std::os::unix::fs::symlink("../top.txt", fx.path("docs/link")).unwrap();
    // A file from elsewhere — another account's, say — carrying an id the base does not know.
    std::fs::write(fx.path("docs/stranger.txt"), b"theirs").unwrap();
    xattr::set(fx.path("docs/stranger.txt"), XATTR_ITEM_ID, b"Y").unwrap();

    let applied = cycle(&fx, &[Change::Delete("D".into())], false).await.unwrap();
    drop(open);
    let mut left: Vec<String> = walk(&fx.path("docs")).into_iter().map(|p| p.strip_prefix(fx.path("docs")).unwrap().display().to_string()).collect();
    left.sort();
    assert_eq!(left, [".~lock.f.txt#", "deep", "deep/mine.txt", "f.txt", "link", "open.txt", "stranger.txt", "zero.txt"], "the rest went");
    assert_eq!(std::fs::read(fx.path("docs/f.txt")).unwrap(), b"one and mine");
    for kept in ["docs", "docs/deep", "docs/f.txt", "docs/open.txt", "docs/zero.txt", "docs/stranger.txt"] {
        assert_eq!(id_at(&fx.path(kept)), None, "{kept} is the user's own now");
    }
    assert_eq!(kept(&applied), vec![(PathBuf::from("docs"), 5, 1)], "said once, for what OneDrive removed");
    let mut recreated = applied.on_disk.recreated.clone();
    recreated.sort();
    assert_eq!(recreated, ["D", "E"], "the folders that stay are made again in OneDrive");
    assert!(applied.on_disk.examine.contains(&(PathBuf::from("docs"), true)), "handed to the examination: {:?}", applied.on_disk.examine);
    assert!(applied.pending.unsettled.is_empty(), "nothing waits: {:?}", applied.pending.unsettled);
    assert!(fx.base("D").is_none() && fx.base("F").is_none() && fx.base("X").is_none(), "gone from the base");
    assert!(fx.deferred("D").is_none() && fx.deferred("F").is_none());
    assert!(fx.path("top.txt").exists());
}

/// What only this computer has stays whatever its name: an ignored file
/// with data, and a directory with an ignored name that holds anything,
/// keep the folder OneDrive removed, and are not counted as uploaded. What
/// holds nothing — an empty ignored file, a symlink — keeps no folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ignored_file_with_data_stays_and_keeps_its_folder() {
    let fx = listed().await;
    std::fs::write(fx.path("docs/draft.swp"), b"unsaved").unwrap();
    std::fs::create_dir(fx.path("docs/cache.tmp")).unwrap();
    std::fs::write(fx.path("docs/cache.tmp/part"), b"data").unwrap();
    std::fs::write(fx.path("docs/deep/.~lock.g.txt#"), b"").unwrap();
    std::os::unix::fs::symlink("../../top.txt", fx.path("docs/deep/link")).unwrap();
    let applied = cycle(&fx, &[Change::Delete("D".into())], false).await.unwrap();
    assert_eq!(std::fs::read(fx.path("docs/draft.swp")).unwrap(), b"unsaved");
    assert_eq!(std::fs::read(fx.path("docs/cache.tmp/part")).unwrap(), b"data");
    assert!(!fx.path("docs/f.txt").exists(), "what OneDrive had went");
    assert!(!fx.path("docs/deep").exists(), "and a folder holding only an empty ignored file and a symlink");
    assert_eq!(kept(&applied), vec![(PathBuf::from("docs"), 0, 2)], "kept, and not said to be uploaded");
}

/// A downloaded file carrying an id that is not of what OneDrive removed —
/// moved in from another konedrive folder, not examined yet — is not a copy
/// of anything this OneDrive had: it stays, though its stamp matches. The
/// removed item's own unchanged download goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_that_is_not_of_what_was_removed_stays() {
    let fx = listed().await;
    write_version(&fx.path("docs/f.txt"), b"one", "c1");
    std::fs::write(fx.path("docs/foreign.txt"), b"").unwrap();
    xattr::set(fx.path("docs/foreign.txt"), XATTR_ITEM_ID, b"Y").unwrap();
    write_version(&fx.path("docs/foreign.txt"), b"theirs", "c9");
    let applied = cycle(&fx, &[Change::Delete("D".into())], false).await.unwrap();
    assert_eq!(std::fs::read(fx.path("docs/foreign.txt")).unwrap(), b"theirs");
    assert_eq!(id_at(&fx.path("docs/foreign.txt")), None, "the user's own now");
    assert!(!fx.path("docs/f.txt").exists(), "the item's own unchanged download went");
    assert_eq!(kept(&applied), vec![(PathBuf::from("docs"), 1, 0)]);
}

/// What stays of what was removed: the place, how many files go up as new,
/// how many items stay on this computer only.
fn kept(applied: &Applied) -> Vec<(PathBuf, u64, u64)> {
    applied.on_disk.kept.iter().map(|(rel, kept)| (rel.clone(), kept.uploaded, kept.local)).collect()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            out.extend(walk(&entry.path()));
        }
        out.push(entry.path());
    }
    out
}

/// Before the reconcile takes anything off the disk,
/// the store forgets the local objects of what it removes — the item, what
/// the base has below it, and an object from elsewhere moved in, by its own
/// id — so that no examination can prove one of them gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_is_removed_is_forgotten_before_it_goes() {
    let fx = listed().await;
    assert!(handle(&fx, "F").await.is_some() && handle(&fx, "T").await.is_some());
    // `top.txt` moved into `docs` here, not examined yet.
    std::fs::rename(fx.path("top.txt"), fx.path("docs/top.txt")).unwrap();
    cycle(&fx, &[Change::Delete("D".into())], false).await.unwrap();
    assert!(!fx.path("docs").exists());
    assert_eq!(handle(&fx, "T").await, None, "the object moved in was taken off too: forgotten");
    let staged = fx.store.call(move |s| s.get(Table::Staging, "T")).await.unwrap();
    assert!(staged.is_some(), "still in OneDrive");
    cycle(&fx, &[], false).await.unwrap();
    assert_eq!(id_at(&fx.path("top.txt")).as_deref(), Some("T"), "placed again where OneDrive has it");
}

/// An object that will not go fails the cycle, as any
/// cycle that cannot do its work; nothing of it is committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_that_fails_fails_the_cycle_and_commits_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let fx = listed().await;
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = cycle(&fx, &[Change::Delete("D".into())], false).await;
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert!(fx.base("D").is_some() && fx.base("G").is_some(), "the base keeps what the disk still has");
}

/// A removal that fails after it stopped a
/// download leaves the file that survives a placeholder again, not partly
/// filled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_that_fails_after_stopping_a_download_leaves_a_placeholder() {
    use std::os::unix::fs::PermissionsExt;
    let fx = listed().await;
    let at = fx.path("docs/deep/g.txt");
    {
        use std::os::unix::fs::FileExt;
        let file = placeholder::reopen_writable(&File::open(&at).unwrap()).unwrap();
        file.write_all_at(&[7u8; 3], 0).unwrap();
        placeholder::write_state(&file, State::Hydrating).unwrap();
    }
    let key = crate::folder::locks::InodeKey::of(&File::open(&at).unwrap()).unwrap();
    let (held, holding) = tokio::sync::oneshot::channel();
    let fill = tokio::spawn({
        let locks = fx.locks.clone();
        async move {
            let guard = locks.lock(key).await;
            held.send(()).unwrap();
            guard.cancelled().await;
        }
    });
    holding.await.unwrap();
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = cycle(&fx, &[Change::Delete("D".into())], false).await;
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err());
    fill.await.unwrap();
    let file = File::open(&at).unwrap();
    assert_eq!(placeholder::read_state(&file).unwrap(), Some(State::OnlineOnly), "a placeholder again");
    let mut left = Vec::new();
    std::io::Read::read_to_end(&mut &file, &mut left).unwrap();
    assert!(left.iter().all(|&b| b == 0), "nothing of the stopped download left: {left:?}");
}

/// A file removed in OneDrive that has a second name here, a hard
/// link the user made. Its name goes first and its item id is taken off
/// afterwards: a removal that fails leaves the item at its place with its id,
/// never an object without one there, which would be uploaded as new. Once
/// the name is gone, the other name is the user's own file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_with_another_name_loses_its_id_only_once_its_name_is_gone() {
    use std::os::unix::fs::PermissionsExt;
    let fx = listed().await;
    write_version(&fx.path("docs/f.txt"), b"one", "c1");
    std::fs::hard_link(fx.path("docs/f.txt"), fx.path("link.txt")).unwrap();
    std::fs::set_permissions(fx.path("docs"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = cycle(&fx, &[Change::Delete("F".into())], false).await;
    std::fs::set_permissions(fx.path("docs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"), "what would not go is still the item");

    cycle(&fx, &[Change::Delete("F".into())], false).await.unwrap();
    assert!(!fx.path("docs/f.txt").exists());
    assert_eq!(std::fs::read(fx.path("link.txt")).unwrap(), b"one", "the other name stays");
    assert_eq!(id_at(&fx.path("link.txt")), None, "as the user's own file");
    assert!(fx.base("F").is_none());
}

/// The stop between the two steps: a file that can no longer be
/// placed has a second name, a hard link the user made; the daemon unlinks
/// its own name and stops before it takes the item id off. The other name
/// is then the user's own file whatever comes next: an examination records
/// no move and no delete of the item for it, and no later cycle removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_after_the_unlink_leaves_the_other_name_to_the_user() {
    use crate::remote::materialize::removal::testing::stop_after_unlink;
    let fx = listed().await;
    write_version(&fx.path("docs/f.txt"), b"one", "c1");
    let leaving = Change::Upsert(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::NameTooLong), ..row("F", "D", &"x".repeat(300), Kind::File, "c1") });
    std::fs::hard_link(fx.path("docs/f.txt"), fx.path("link.txt")).unwrap();

    stop_after_unlink(&fx.root.path, true);
    // The pass that stops fails, and the Full pass that follows it in the
    // same reconcile is already "what comes next".
    let stopped = cycle(&fx, std::slice::from_ref(&leaving), false).await;
    stop_after_unlink(&fx.root.path, false);
    assert!(!fx.path("docs/f.txt").exists(), "stopped right after the unlink: {stopped:?}");
    assert_eq!(id_at(&fx.path("link.txt")).as_deref(), Some("F"), "the id was not taken off");

    // An examination of the whole folder, on a plain thread: no row for the
    // item, and the other name is not recorded as its object.
    let examine = || {
        konedrive_tree::off_runtime(|| {
            let disk = Disk::open(&fx.root, false).unwrap();
            let now = crate::remote::testing::now();
            crate::local::Examiner { disk: &disk, store: &fx.store, liveness: &crate::local::liveness::NoLiveness, ignore: &IgnoreList::default(), locks: &fx.locks, now }
                .examine(&crate::local::Batch::full())
                .unwrap();
            let rows = fx.store.call_blocking(|s| s.outbox_rows()).unwrap();
            assert!(!rows.iter().any(|r| r.item_id.as_deref() == Some("F")), "nothing is sent for the item: {rows:?}");
            assert_eq!(fx.store.call_blocking(|s| s.local_handle("F")).unwrap(), None, "and the other name is not recorded as the item's object");
        })
    };
    examine();
    for full in [true, false] {
        cycle(&fx, &[], full).await.unwrap();
        examine();
        assert_eq!(std::fs::read(fx.path("link.txt")).unwrap(), b"one", "full={full}: the other name is never removed");
    }
}

/// An item OneDrive has under the outbox's temporary name (a store
/// rebuilt before its final move) keeps its local object where it is: the
/// examination moves it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_under_a_temporary_name_in_onedrive_stays_where_it_is_here() {
    let fx = listed().await;
    let swapped = Change::Upsert(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::ReservedName), ..row("F", "D", ".konedrive-swap-F", Kind::File, "c1") });
    cycle(&fx, &[swapped], true).await.unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(fx.base("F").unwrap().name, ".konedrive-swap-F", "the base says where it is in OneDrive");
}

/// A stop left a new folder under its temporary name, and
/// OneDrive removed its item before the next cycle. The folder was only ever
/// the daemon's: it goes, never to come back as a folder named after the
/// item id and uploaded; what someone put in it is put back where it stood.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_folder_a_stop_left_under_its_temporary_name_is_finished_when_onedrive_removed_it() {
    for with_a_file in [false, true] {
        let fx = listed().await;
        std::fs::create_dir(fx.path("docs/.konedrive-new-N")).unwrap();
        xattr::set(fx.path("docs/.konedrive-new-N"), XATTR_ITEM_ID, b"N").unwrap();
        if with_a_file {
            std::fs::write(fx.path("docs/.konedrive-new-N/mine.txt"), b"mine").unwrap();
        }
        let applied = cycle(&fx, &[], true).await.unwrap();
        assert!(!fx.path("N").exists() && !fx.path("docs/N").exists(), "with_a_file={with_a_file}: {:?}", applied.on_disk.examine);
        assert!(!fx.path("docs/.konedrive-new-N").exists() && !fx.path(".konedrive-holding").exists());
        assert!(applied.on_disk.rescued.is_empty());
        assert_eq!(fx.path("docs/mine.txt").exists(), with_a_file, "what was in it is put back where it stood");
        assert!(applied.on_disk.examine.iter().all(|(rel, _)| rel == Path::new("docs/mine.txt")), "{:?}", applied.on_disk.examine);
    }
}

/// Where a misplaced object was, by its item's plan alone: each of the six
/// answers, for a file found at `docs/a.txt` in the folder `D`, or away
/// from there.
#[test]
fn where_a_misplaced_object_was_is_read_off_its_plan() {
    use super::sort::{where_it_was, Was};
    use crate::folder::disk::Scanned;
    use konedrive_tree::outbox::SWAP_PREFIX;
    use konedrive_tree::{Located, Planned, Side, SkipReason};

    let side = |row: Row, rel: &str, placed: bool| Some(Side { row, at: Some(Located { rel: rel.into(), placed, depth: rel.split('/').count() }) });
    let at_base = Scanned { rel: "docs/a.txt".into(), id: Some("A".into()), is_dir: false, depth: 2, parent_id: Some("D".into()) };
    let away = Scanned { rel: "other/a.txt".into(), id: Some("A".into()), is_dir: false, depth: 2, parent_id: Some("O".into()) };
    let base = || side(row("A", "D", "a.txt", Kind::File, "c1"), "docs/a.txt", true);
    let skipped = || Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..row("A", "D", "a.txt", Kind::File, "c1") };
    let moved = || side(row("A", "R", "a.txt", Kind::File, "c1"), "a.txt", true);

    let cases = [
        ("moved in OneDrive", &at_base, Planned { base: base(), new: moved() }, Was::Moved),
        ("left by a placement stopped before its swap", &at_base, Planned { base: None, new: moved() }, Was::Moved),
        ("removed in OneDrive", &at_base, Planned { base: base(), new: None }, Was::Removed),
        ("removed in OneDrive, not placed by the base", &away, Planned { base: side(skipped(), "docs/a.txt", false), new: None }, Was::Removed),
        ("no longer placed", &at_base, Planned { base: base(), new: side(skipped(), "docs/a.txt", false) }, Was::Unplaced),
        ("still not placed, where the base has it", &at_base, Planned { base: side(skipped(), "docs/a.txt", false), new: side(skipped(), "docs/a.txt", false) }, Was::Unplaced),
        ("moved here, and moved in OneDrive", &away, Planned { base: base(), new: moved() }, Was::Elsewhere),
        ("moved here, and removed in OneDrive", &away, Planned { base: base(), new: None }, Was::Elsewhere),
        ("moved out of what is not placed", &away, Planned { base: side(skipped(), "docs/a.txt", false), new: side(skipped(), "docs/a.txt", false) }, Was::Elsewhere),
        ("under the outbox's temporary name", &at_base, Planned { base: base(), new: side(row("A", "D", &format!("{SWAP_PREFIX}a.txt"), Kind::File, "c1"), "docs/swap", true) }, Was::Swapped),
        ("an id nobody has", &at_base, Planned { base: None, new: None }, Was::Stranger),
        ("new in OneDrive and not placed", &at_base, Planned { base: None, new: side(skipped(), "docs/a.txt", false) }, Was::Stranger),
    ];
    for (what, entry, planned, was) in cases {
        assert_eq!(where_it_was(entry, &planned), was, "{what}");
    }
}
