//! Read-write mode's reconcile rules (`docs/design/writes.md` §9, §7), in
//! temporary directories: the folder is placed by the read phase's
//! materializer, changed by hand, and reconciled with a delta as a
//! read-write cycle does it — deferring what the disk does not show.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_ROOT};
use tokio_util::sync::CancellationToken;

use super::Rw;
use crate::folder::disk::Disk;
use crate::local::IgnoreList;
use crate::remote::materialize::{Applied, ApplyError, Materializer, Scope};
use crate::folder::root::SyncRoot;
use crate::folder::locks::InodeLocks;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxState};
use konedrive_tree::{Change, Kind, Placement, Row, Store, Table, TreeStore};

struct Fx {
    _dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    rescue: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
    locks: InodeLocks,
}

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

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&path).unwrap();
        let root_id = "2c9d1e6f-7a3b-4c5d-8e9f-0a1b2c3d4e5f".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let fx = Fx {
            _dir: dir,
            root: SyncRoot { path, root_id },
            store: Store::new(TreeStore::in_memory().unwrap()),
            rescue: tempfile::tempdir().unwrap(),
            runtime: tokio::runtime::Runtime::new().unwrap(),
            locks: InodeLocks::new(),
        };
        fx.store.call_blocking(move |s| {
            s.begin_staging(konedrive_tree::NewTree::Whole)?;
            s.stage(&tree())
        })
        .unwrap();
        fx.materializer(None).apply(Scope::Full).unwrap();
        fx.store.call_blocking(move |s| s.commit_staging("link-1")).unwrap();
        fx
    }

    fn materializer(&self, rw: Option<Rw>) -> Materializer {
        Materializer {
            disk: Disk::open(&self.root, false).unwrap(),
            store: self.store.clone(),
            link: None,
            runtime: self.runtime.handle().clone(),
            locks: self.locks.clone(),
            root_item_id: "R".into(),
            rescue_into: self.rescue.path().join("now"),
            cancel: CancellationToken::new(),
            rw,
            claimed: None,
        }
    }

    /// `changes` staged over the base and reconciled in read-write mode —
    /// Full, or Changed over what they change and what has no local object —
    /// and committed as a read-write cycle commits: what the disk does not
    /// show keeps its base, and its change waits.
    fn cycle(&self, changes: &[Change], full: bool) -> Result<Applied, ApplyError> {
        let staged = changes.to_vec();
        let (ids, plan) = self
            .store
            .call_blocking(move |s| {
                s.begin_staging(konedrive_tree::NewTree::Delta)?;
                s.stage(&staged)?;
                let mut ids = s.changed_ids()?;
                ids.extend(s.unplaced(Table::Staging)?);
                Ok((ids, Rw::read(s, "fedora".into(), false, IgnoreList::default())?))
            })
            .unwrap();
        let scope = if full { Scope::Full } else { Scope::Changed(ids) };
        let applied = self.materializer(Some(plan.clone())).apply(scope)?;
        let changed = self.store.call_blocking(move |s| s.changed_ids()).unwrap();
        let defer: Vec<String> = changed
            .iter()
            .filter(|id| !plan.removing.contains(*id) && !applied.on_disk.taken.contains(*id) && (plan.held.contains(*id) || applied.pending.unsettled.contains(*id)))
            .cloned()
            .collect();
        let content: Vec<String> = changed
            .iter()
            .filter(|id| !plan.removing.contains(*id) && !applied.on_disk.taken.contains(*id) && !defer.contains(id) && applied.pending.content_waits.contains(*id))
            .cloned()
            .collect();
        self.store.call_blocking(move |s| s.commit_staging_deferring("link-2", &konedrive_tree::reconcile::Deferrals { consumed: &[], whole: &defer, content: &content, fetched_at: 0 })).unwrap();
        Ok(applied)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    /// A live outbox row of `kind` for `id` (None: something new) at `rel`.
    fn row(&self, kind: OutboxKind, id: Option<&str>, rel: &str) {
        let base = id.and_then(|id| { let id = id.to_owned(); self.store.call_blocking(move |s| s.get(Table::Items, &id)).unwrap() }).map(|r| Base {
            etag: r.etag,
            ctag: r.ctag,
            parent: r.parent_id,
            name: Some(r.name),
        });
        let rel = PathBuf::from(rel);
        let detection = Detection {
            kind,
            item_id: id.map(str::to_owned),
            inode: None,
            target_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()),
            rel,
            base,
            target_parent: Some("D".into()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        };
        self.store.call_blocking(move |s| s.outbox_record(&detection)).unwrap();
    }

    fn base(&self, id: &str) -> Option<Row> {
        { let id = id.to_owned(); self.store.call_blocking(move |s| s.get(Table::Items, &id)).unwrap() }
    }

    fn deferred(&self, id: &str) -> Option<Change> {
        { let id = id.to_owned(); self.store.call_blocking(move |s| s.deferred(&id)).unwrap() }
    }
}

fn id_at(path: &Path) -> Option<String> {
    xattr::get(path, XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap())
}

/// Downloads the placeholder at `at` by hand: `content`, at version `ctag`.
fn hydrate(at: &Path, content: &[u8], ctag: &str) {
    use std::os::unix::fs::FileExt;
    let file = placeholder::reopen_writable(&File::open(at).unwrap()).unwrap();
    file.set_len(content.len() as u64).unwrap();
    file.write_all_at(content, 0).unwrap();
    placeholder::write_ctag(&file, ctag).unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
}

/// An edit made here: the stamp no longer matches.
fn edit(at: &Path, more: &[u8]) {
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(at).unwrap().write_all(more).unwrap();
}

/// §3.7: an item of ours away from where the base has it — a local move the
/// examination has not seen yet — is left where it is, never made again
/// where the base has it; the delta's change to it waits, and the base keeps
/// the version the file holds.
#[test]
fn a_local_move_not_examined_yet_is_left_where_it_is_and_its_change_waits() {
    let fx = Fx::new();
    std::fs::rename(fx.path("docs/f.txt"), fx.path("docs/moved.txt")).unwrap();
    let applied = fx.cycle(&[file("F", "D", "f.txt", "c2")], true).unwrap();
    assert_eq!(id_at(&fx.path("docs/moved.txt")).as_deref(), Some("F"), "left where the user put it");
    assert!(!fx.path("docs/f.txt").exists(), "not made again at its old name");
    assert!(applied.pending.unsettled.contains("F"));
    assert_eq!(fx.base("F").unwrap().ctag.as_deref(), Some("c1"), "the base keeps what the file holds");
    assert_eq!(fx.deferred("F"), Some(file("F", "D", "f.txt", "c2")), "OneDrive's change waits");
}

/// §3.7: an item with a live outbox row — in any state — is not moved,
/// replaced or removed, nor is anything below a folder a row moves; their
/// changes wait. A disagreement those rows explain never turns a Changed
/// scope Full. Below a folder a `delete` row removes, nothing is placed, and
/// the delta goes to the base (the read-write reconcile must, item 3).
#[test]
fn rows_keep_the_reconcile_off_their_items_and_a_changed_scope_does_not_turn_full() {
    let fx = Fx::new();
    fx.row(OutboxKind::Update, Some("F"), "docs/f.txt");
    std::fs::rename(fx.path("docs/deep"), fx.path("docs/deeper")).unwrap();
    fx.row(OutboxKind::Move, Some("E"), "docs/deeper");
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    fx.row(OutboxKind::Delete, Some("T"), "top.txt");
    let changes = [file("F", "D", "renamed.txt", "c2"), file("N", "E", "n.txt", "c1"), file("T", "R", "top.txt", "c2")];
    let applied = fx.cycle(&changes, false).expect("no Full reconcile over rows");
    assert!(fx.path("docs/f.txt").exists() && !fx.path("docs/renamed.txt").exists(), "not moved under its row");
    assert!(!fx.path("docs/deeper/n.txt").exists() && !fx.path("docs/deep").exists(), "nothing placed where a moving folder was");
    assert!(!fx.path("top.txt").exists(), "a delete row's item is not placed again by the reconcile");
    assert_eq!(fx.base("F").unwrap().name, "f.txt");
    assert!(fx.deferred("F").is_some() && fx.deferred("N").is_some(), "{:?}", applied.pending.unsettled);
    assert_eq!(fx.base("T").unwrap().ctag.as_deref(), Some("c2"), "below a removal the delta goes to the base");
    assert!(fx.deferred("T").is_none());
}

/// §3.7: a tree item missing here is made again only with something to
/// place — new, or with no local object on record (the outbox forgot it once
/// OneDrive's change won, delete × edit, §6). One whose local object is on
/// record and changed in OneDrive is not placed again, in either scope: its
/// object may be alive out of the folder, and the
/// examination decides first, from its base place.
#[test]
fn a_missing_item_is_placed_again_only_with_something_to_place() {
    let fx = Fx::new();
    std::fs::remove_file(fx.path("docs/f.txt")).unwrap();
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    for full in [false, true] {
        let applied = fx.cycle(&[file("T", "R", "top.txt", "c2")], full).expect("never a hand-over for it");
        assert!(!fx.path("top.txt").exists(), "full={full}: not placed while its object may be alive");
        assert!(applied.pending.unsettled.contains("T") && fx.deferred("T").is_some(), "its change waits");
        assert!(applied.on_disk.examine.contains(&(PathBuf::from("top.txt"), false)), "the examination decides: {:?}", applied.on_disk.examine);
    }
    assert!(!fx.path("docs/f.txt").exists(), "a delete not examined yet is not undone");

    // The outbox forgets the object once OneDrive's change won: then it comes back.
    fx.store.call_blocking(move |s| s.set_local_handle("T", None)).unwrap();
    fx.cycle(&[file("T", "R", "top.txt", "c2")], false).unwrap();
    assert_eq!(id_at(&fx.path("top.txt")).as_deref(), Some("T"), "changed in OneDrive: it comes back");
    assert_eq!(placeholder::read_state(&File::open(fx.path("top.txt")).unwrap()).unwrap(), Some(State::OnlineOnly));

    // F82 (8): an item the outbox forgot (its local object dropped) is placed again.
    fx.store.call_blocking(move |s| s.set_local_handle("F", None)).unwrap();
    fx.cycle(&[], false).unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert!(fx.store.call_blocking(move |s| s.local_handle("F")).unwrap().is_some(), "and its object recorded again");
}

/// §3.7, §6 create/create: a file of the user's where a new item arrives is
/// kept beside it, `name-<machine>`, stripped, for the outbox to upload; the
/// item takes the name. A pending `create` there is the outbox's to settle:
/// the item waits. And a save not examined yet at an item OneDrive did not
/// change is the user's, not a conflict.
#[test]
fn a_local_file_in_the_way_is_kept_beside_the_clouds_but_a_pending_create_is_left_to_the_outbox() {
    let fx = Fx::new();
    std::fs::write(fx.path("docs/new.txt"), b"mine").unwrap();
    let applied = fx.cycle(&[file("N", "D", "new.txt", "c1")], false).unwrap();
    assert_eq!(std::fs::read(fx.path("docs/new-fedora.txt")).unwrap(), b"mine");
    assert_eq!(id_at(&fx.path("docs/new-fedora.txt")), None);
    assert_eq!(id_at(&fx.path("docs/new.txt")).as_deref(), Some("N"));
    assert_eq!(applied.on_disk.copies.len(), 1);
    assert!(applied.on_disk.examine.contains(&(PathBuf::from("docs/new-fedora.txt"), false)));

    std::fs::write(fx.path("docs/other.txt"), b"mine too").unwrap();
    fx.row(OutboxKind::Create, None, "docs/other.txt");
    let applied = fx.cycle(&[file("O", "D", "other.txt", "c1")], false).unwrap();
    assert_eq!(std::fs::read(fx.path("docs/other.txt")).unwrap(), b"mine too", "the outbox settles create/create");
    assert!(applied.on_disk.copies.is_empty() && applied.pending.unsettled.contains("O"));
    assert!(fx.base("O").is_none() && fx.deferred("O").is_some(), "the new item waits");

    // A save by rename (the placeholder replaced by a new file): OneDrive
    // changed nothing, so the file stays as it is.
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    std::fs::write(fx.path("top.txt"), b"saved").unwrap();
    let applied = fx.cycle(&[], true).unwrap();
    assert_eq!(std::fs::read(fx.path("top.txt")).unwrap(), b"saved");
    assert!(applied.on_disk.copies.is_empty());

    // M3: a folder made here where a new one arrives from OneDrive is left
    // for the two to merge (the `mkdir`'s `409`), never copied aside.
    std::fs::create_dir(fx.path("photos")).unwrap();
    std::fs::write(fx.path("photos/mine.jpg"), b"jpg").unwrap();
    let applied = fx.cycle(&[folder("P", "R", "photos"), file("Q", "P", "theirs.jpg", "c1")], false).unwrap();
    assert!(applied.on_disk.copies.is_empty() && !fx.path("photos-fedora").exists());
    assert_eq!(id_at(&fx.path("photos")), None);
    assert!(fx.path("photos/mine.jpg").exists() && !fx.path("photos/theirs.jpg").exists());
    assert!(fx.deferred("P").is_some() && fx.deferred("Q").is_some(), "they wait for the merge");
}

/// §6 edit × edit, found by the reconcile: the changed download is kept as
/// `name-<machine>`, stripped, and OneDrive's version takes the name.
#[test]
fn an_edit_here_and_in_onedrive_keeps_both() {
    let fx = Fx::new();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    edit(&fx.path("docs/f.txt"), b" and mine");
    let applied = fx.cycle(&[file("F", "D", "f.txt", "c2")], false).unwrap();
    assert_eq!(std::fs::read(fx.path("docs/f-fedora.txt")).unwrap(), b"one and mine");
    assert_eq!(id_at(&fx.path("docs/f-fedora.txt")), None, "the user's own file now");
    assert_eq!(placeholder::read_ctag(&File::open(fx.path("docs/f.txt")).unwrap()).unwrap().as_deref(), Some("c2"));
    assert_eq!(applied.on_disk.copies[0].copy, PathBuf::from("docs/f-fedora.txt"));
    assert_eq!(fx.base("F").unwrap().ctag.as_deref(), Some("c2"));
}

/// What OneDrive removed (the owner's ruling of 2026-10-04, which replaced
/// decision 2 of issue #104): what OneDrive had and the daemon placed goes
/// — files not downloaded, a download unchanged since, a folder with
/// nothing left in it. What OneDrive never had stays as the user's own,
/// its attributes off, with the folders above it: a file made here, a
/// download changed here, emptied here, or open for writing, a file from
/// elsewhere holding data, an ignored file with data. A symlink stays beside
/// them. The base takes the removal, and nothing waits.
#[test]
fn what_onedrive_removed_keeps_only_what_it_never_had() {
    let fx = Fx::new();
    fx.cycle(&[folder("X", "D", "empty"), file("H", "D", "h.txt", "c1"), file("O", "D", "open.txt", "c1"), file("Z", "D", "zero.txt", "c1")], false).unwrap();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    edit(&fx.path("docs/f.txt"), b" and mine");
    hydrate(&fx.path("docs/h.txt"), b"one", "c1");
    hydrate(&fx.path("docs/open.txt"), b"one", "c1");
    let open = std::fs::OpenOptions::new().read(true).write(true).open(fx.path("docs/open.txt")).unwrap();
    hydrate(&fx.path("docs/zero.txt"), b"one", "c1");
    std::thread::sleep(std::time::Duration::from_millis(10));
    File::options().write(true).truncate(true).open(fx.path("docs/zero.txt")).unwrap();
    std::fs::write(fx.path("docs/deep/mine.txt"), b"new here").unwrap();
    std::fs::write(fx.path("docs/.~lock.f.txt#"), b"lock").unwrap();
    std::os::unix::fs::symlink("../top.txt", fx.path("docs/link")).unwrap();
    // A file from elsewhere — another account's, say — carrying an id the base does not know.
    std::fs::write(fx.path("docs/stranger.txt"), b"theirs").unwrap();
    xattr::set(fx.path("docs/stranger.txt"), XATTR_ITEM_ID, b"Y").unwrap();

    let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
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
#[test]
fn an_ignored_file_with_data_stays_and_keeps_its_folder() {
    let fx = Fx::new();
    std::fs::write(fx.path("docs/draft.swp"), b"unsaved").unwrap();
    std::fs::create_dir(fx.path("docs/cache.tmp")).unwrap();
    std::fs::write(fx.path("docs/cache.tmp/part"), b"data").unwrap();
    std::fs::write(fx.path("docs/deep/.~lock.g.txt#"), b"").unwrap();
    std::os::unix::fs::symlink("../../top.txt", fx.path("docs/deep/link")).unwrap();
    let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
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
#[test]
fn a_download_that_is_not_of_what_was_removed_stays() {
    let fx = Fx::new();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    std::fs::write(fx.path("docs/foreign.txt"), b"").unwrap();
    xattr::set(fx.path("docs/foreign.txt"), XATTR_ITEM_ID, b"Y").unwrap();
    hydrate(&fx.path("docs/foreign.txt"), b"theirs", "c9");
    let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
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

/// Issue #104, decision 5: before the reconcile takes anything off the disk,
/// the store forgets the local objects of what it removes — the item, what
/// the base has below it, and an object from elsewhere moved in, by its own
/// id — so that no examination can prove one of them gone.
#[test]
fn what_is_removed_is_forgotten_before_it_goes() {
    let fx = Fx::new();
    let handle = |id: &str| { let id = id.to_owned(); fx.store.call_blocking(move |s| s.local_handle(&id)).unwrap() };
    assert!(handle("F").is_some() && handle("T").is_some());
    // `top.txt` moved into `docs` here, not examined yet.
    std::fs::rename(fx.path("top.txt"), fx.path("docs/top.txt")).unwrap();
    fx.cycle(&[Change::Delete("D".into())], false).unwrap();
    assert!(!fx.path("docs").exists());
    assert_eq!(handle("T"), None, "the object moved in was taken off too: forgotten");
    let staged = fx.store.call_blocking(move |s| s.get(Table::Staging, "T")).unwrap();
    assert!(staged.is_some(), "still in OneDrive");
    fx.cycle(&[], false).unwrap();
    assert_eq!(id_at(&fx.path("top.txt")).as_deref(), Some("T"), "placed again where OneDrive has it");
}

/// Issue #104, point 3: an object that will not go fails the cycle, as any
/// cycle that cannot do its work; nothing of it is committed.
#[test]
fn a_removal_that_fails_fails_the_cycle_and_commits_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = fx.cycle(&[Change::Delete("D".into())], false);
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert!(fx.base("D").is_some() && fx.base("G").is_some(), "the base keeps what the disk still has");
}

/// Review fixes, round 2, point 6: a removal that fails after it stopped a
/// download leaves the file that survives a placeholder again, not partly
/// filled.
#[test]
fn a_removal_that_fails_after_stopping_a_download_leaves_a_placeholder() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let at = fx.path("docs/deep/g.txt");
    {
        use std::os::unix::fs::FileExt;
        let file = placeholder::reopen_writable(&File::open(&at).unwrap()).unwrap();
        file.write_all_at(&[7u8; 3], 0).unwrap();
        placeholder::write_state(&file, State::Hydrating).unwrap();
    }
    let key = crate::folder::locks::InodeKey::of(&File::open(&at).unwrap()).unwrap();
    let (held, holding) = std::sync::mpsc::channel();
    let fill = fx.runtime.spawn({
        let locks = fx.locks.clone();
        async move {
            let guard = locks.lock(key).await;
            held.send(()).unwrap();
            guard.cancelled().await;
        }
    });
    holding.recv().unwrap();
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = fx.cycle(&[Change::Delete("D".into())], false);
    std::fs::set_permissions(fx.path("docs/deep"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err());
    fx.runtime.block_on(fill).unwrap();
    let file = File::open(&at).unwrap();
    assert_eq!(placeholder::read_state(&file).unwrap(), Some(State::OnlineOnly), "a placeholder again");
    let mut left = Vec::new();
    std::io::Read::read_to_end(&mut &file, &mut left).unwrap();
    assert!(left.iter().all(|&b| b == 0), "nothing of the stopped download left: {left:?}");
}

/// Issue #112: a file removed in OneDrive that has a second name here, a hard
/// link the user made. Its name goes first and its item id is taken off
/// afterwards: a removal that fails leaves the item at its place with its id,
/// never an object without one there, which would be uploaded as new. Once
/// the name is gone, the other name is the user's own file.
#[test]
fn a_file_with_another_name_loses_its_id_only_once_its_name_is_gone() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    std::fs::hard_link(fx.path("docs/f.txt"), fx.path("link.txt")).unwrap();
    std::fs::set_permissions(fx.path("docs"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = fx.cycle(&[Change::Delete("F".into())], false);
    std::fs::set_permissions(fx.path("docs"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"), "what would not go is still the item");

    fx.cycle(&[Change::Delete("F".into())], false).unwrap();
    assert!(!fx.path("docs/f.txt").exists());
    assert_eq!(std::fs::read(fx.path("link.txt")).unwrap(), b"one", "the other name stays");
    assert_eq!(id_at(&fx.path("link.txt")), None, "as the user's own file");
    assert!(fx.base("F").is_none());
}

/// Issue #112, the stop between the two steps: a file that is leaving has a
/// second name, a hard link the user made; the daemon unlinks the leaving
/// name and stops before it takes the item id off. The other name is then
/// the user's own file whatever comes next: an examination records no move
/// and no delete of the item for it, and no later cycle takes it for the
/// leaving object and removes it.
#[test]
fn a_stop_after_the_unlink_leaves_the_other_name_to_the_user() {
    use crate::remote::materialize::removal::testing::stop_after_unlink;
    let fx = Fx::new();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    let leaving = Change::Upsert(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::NameTooLong), ..row("F", "D", &"x".repeat(300), Kind::File, "c1") });
    fx.cycle(std::slice::from_ref(&leaving), false).unwrap();
    assert_eq!(fx.store.call_blocking(|s| s.leaving()).unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))]);
    std::fs::hard_link(fx.path("docs/f.txt"), fx.path("link.txt")).unwrap();

    stop_after_unlink(true);
    let stopped = fx.cycle(&[], false);
    stop_after_unlink(false);
    assert!(stopped.is_err() && !fx.path("docs/f.txt").exists(), "stopped right after the unlink: {stopped:?}");
    assert_eq!(id_at(&fx.path("link.txt")).as_deref(), Some("F"), "the id was not taken off");

    let examine = || {
        let disk = Disk::open(&fx.root, false).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        crate::local::Examiner { disk: &disk, store: &fx.store, liveness: &crate::local::NoLiveness, ignore: &IgnoreList::default(), locks: &fx.locks, now }
            .examine(&crate::local::Batch::full())
            .unwrap();
        let rows = fx.store.call_blocking(|s| s.outbox_rows()).unwrap();
        assert!(!rows.iter().any(|r| r.item_id.as_deref() == Some("F")), "nothing is sent for the item: {rows:?}");
        assert_eq!(fx.store.call_blocking(|s| s.local_handle("F")).unwrap(), None, "and the other name is not recorded as the item's object");
    };
    examine();
    for full in [true, false] {
        fx.cycle(&[], full).unwrap();
        examine();
        assert_eq!(std::fs::read(fx.path("link.txt")).unwrap(), b"one", "full={full}: the other name is never removed");
    }
}

/// F82 (5): an item OneDrive has under the outbox's temporary name (a store
/// rebuilt before its final move) keeps its local object where it is: the
/// examination moves it back.
#[test]
fn an_item_under_a_temporary_name_in_onedrive_stays_where_it_is_here() {
    let fx = Fx::new();
    let swapped = Change::Upsert(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::ReservedName), ..row("F", "D", ".konedrive-swap-F", Kind::File, "c1") });
    fx.cycle(&[swapped], true).unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(fx.base("F").unwrap().name, ".konedrive-swap-F", "the base says where it is in OneDrive");
}

/// A stop left a new folder under its temporary name, and
/// OneDrive removed its item before the next cycle. The folder was only ever
/// the daemon's: it goes, never to come back as a folder named after the
/// item id and uploaded; what someone put in it is put back where it stood.
#[test]
fn a_new_folder_a_stop_left_under_its_temporary_name_is_finished_when_onedrive_removed_it() {
    for with_a_file in [false, true] {
        let fx = Fx::new();
        std::fs::create_dir(fx.path("docs/.konedrive-new-N")).unwrap();
        xattr::set(fx.path("docs/.konedrive-new-N"), XATTR_ITEM_ID, b"N").unwrap();
        if with_a_file {
            std::fs::write(fx.path("docs/.konedrive-new-N/mine.txt"), b"mine").unwrap();
        }
        let applied = fx.cycle(&[], true).unwrap();
        assert!(!fx.path("N").exists() && !fx.path("docs/N").exists(), "with_a_file={with_a_file}: {:?}", applied.on_disk.examine);
        assert!(!fx.path("docs/.konedrive-new-N").exists() && !fx.path(".konedrive-holding").exists());
        assert!(applied.on_disk.rescued.is_empty());
        assert_eq!(fx.path("docs/mine.txt").exists(), with_a_file, "what was in it is put back where it stood");
        assert!(applied.on_disk.examine.iter().all(|(rel, _)| rel == Path::new("docs/mine.txt")), "{:?}", applied.on_disk.examine);
    }
}

/// Fifth review, point 2: the walk for a leaving object whose path is gone
/// meets a directory it cannot list — here the folder itself, open already
/// and no longer readable — and decides nothing: the leaving row stays.
#[test]
fn a_directory_the_walk_cannot_list_keeps_the_leaving_row() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let handle = konedrive_fs::handle::FileHandle::of(&File::open(fx.path("docs/deep")).unwrap()).unwrap();
    fx.store.call_blocking(move |s| s.leaving_add("E", Path::new("docs/deep"), Some(&handle))).unwrap();
    std::fs::rename(fx.path("docs/deep"), fx.path("docs/deeper")).unwrap();
    let plan = fx.store.call_blocking(|s| {
        s.begin_staging(konedrive_tree::NewTree::Delta)?;
        Rw::read(s, "fedora".into(), false, IgnoreList::default())
    }).unwrap();
    let materializer = fx.materializer(Some(plan));
    // Lookups by name work, listing does not.
    std::fs::set_permissions(&fx.root.path, std::fs::Permissions::from_mode(0o300)).unwrap();
    let applied = materializer.apply(Scope::Changed(Vec::new()));
    std::fs::set_permissions(&fx.root.path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = applied;
    let leaving = fx.store.call_blocking(|s| s.leaving()).unwrap();
    assert_eq!(leaving, vec![("E".to_owned(), PathBuf::from("docs/deep"))], "kept");
}

/// RE1: a directory kept aside under another name (`copy_aside`) takes what is leaving
/// in it along, as it takes its outbox rows: the rebase of the rows is the store's, which
/// moves the `leaving` rows at and below the directory too. What is leaving elsewhere
/// stays where it is.
#[test]
fn a_directory_kept_aside_takes_what_is_leaving_in_it_along() {
    let fx = Fx::new();
    fx.store
        .call_blocking(|s| {
            s.leaving_add("G", Path::new("docs/deep/g.txt"), None)?;
            s.leaving_add("E", Path::new("docs/deep"), None)?;
            s.leaving_add("T", Path::new("top.txt"), None)
        })
        .unwrap();
    fx.row(OutboxKind::Update, Some("F"), "docs/f.txt");
    let plan = fx.store.call_blocking(|s| {
        s.begin_staging(konedrive_tree::NewTree::Delta)?;
        Rw::read(s, "fedora".into(), false, IgnoreList::default())
    }).unwrap();
    let materializer = fx.materializer(Some(plan.clone()));
    let top = materializer.disk.dir(Path::new("")).unwrap();
    let mut run = crate::remote::materialize::Run::default();
    materializer.copy_aside(&plan, &top, std::ffi::OsStr::new("docs"), Path::new("docs"), &mut run).unwrap();

    assert!(fx.path("docs-fedora/deep/g.txt").exists() && !fx.path("docs").exists(), "kept aside as docs-fedora");
    let leaving = fx.store.call_blocking(|s| s.leaving()).unwrap();
    assert_eq!(
        leaving,
        vec![
            ("E".to_owned(), PathBuf::from("docs-fedora/deep")),
            ("G".to_owned(), PathBuf::from("docs-fedora/deep/g.txt")),
            ("T".to_owned(), PathBuf::from("top.txt")),
        ]
    );
    let rows: Vec<PathBuf> = fx.store.call_blocking(|s| s.outbox_rows()).unwrap().into_iter().map(|r| r.rel).collect();
    assert_eq!(rows, vec![PathBuf::from("docs-fedora/f.txt")], "its rows follow it too");
}
