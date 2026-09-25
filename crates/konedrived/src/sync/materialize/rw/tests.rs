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
use crate::sync::disk::Disk;
use crate::sync::local::IgnoreList;
use crate::sync::materialize::{Applied, ApplyError, Materializer, Scope};
use crate::sync::root::SyncRoot;
use crate::sync::InodeLocks;
use crate::tree::outbox::{Base, Detection, OutboxKind, OutboxState};
use crate::tree::{Change, Kind, Placement, Row, Store, Table, TreeStore};

struct Fx {
    _dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    rescue: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
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
        };
        fx.store.with(|s| {
            s.begin_staging(false)?;
            s.stage(&tree())
        })
        .unwrap();
        fx.materializer(None).apply(Scope::Full).unwrap();
        fx.store.with(|s| s.commit_staging("link-1")).unwrap();
        fx
    }

    fn materializer(&self, rw: Option<Rw>) -> Materializer {
        Materializer {
            disk: Disk::open(&self.root, false).unwrap(),
            store: self.store.clone(),
            link: None,
            runtime: self.runtime.handle().clone(),
            locks: InodeLocks::new(),
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
        let (ids, plan) = self
            .store
            .with(|s| {
                s.begin_staging(true)?;
                s.stage(changes)?;
                let mut ids = s.changed_ids()?;
                ids.extend(s.unplaced(Table::Staging)?);
                Ok((ids, Rw::read(s, "fedora".into(), false, IgnoreList::default())?))
            })
            .unwrap();
        let scope = if full { Scope::Full } else { Scope::Changed(ids) };
        let applied = self.materializer(Some(plan.clone())).apply(scope)?;
        let changed = self.store.with(|s| s.changed_ids()).unwrap();
        let defer: Vec<String> = changed
            .iter()
            .filter(|id| !plan.removing.contains(*id) && (plan.held.contains(*id) || applied.unsettled.contains(*id)))
            .cloned()
            .collect();
        let content: Vec<String> = changed
            .iter()
            .filter(|id| !plan.removing.contains(*id) && !defer.contains(id) && applied.content_waits.contains(*id))
            .cloned()
            .collect();
        self.store.with(|s| s.commit_staging_deferring("link-2", &[], &defer, &content, 0)).unwrap();
        Ok(applied)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    /// A live outbox row of `kind` for `id` (None: something new) at `rel`.
    fn row(&self, kind: OutboxKind, id: Option<&str>, rel: &str) {
        let base = id.and_then(|id| self.store.with(|s| s.get(Table::Items, id)).unwrap()).map(|r| Base {
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
        };
        self.store.with(|s| s.outbox_record(&detection)).unwrap();
    }

    fn base(&self, id: &str) -> Option<Row> {
        self.store.with(|s| s.get(Table::Items, id)).unwrap()
    }

    fn deferred(&self, id: &str) -> Option<Change> {
        self.store.with(|s| s.deferred(id)).unwrap()
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
    assert!(applied.unsettled.contains("F"));
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
    assert!(fx.deferred("F").is_some() && fx.deferred("N").is_some(), "{:?}", applied.unsettled);
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
        assert!(applied.unsettled.contains("T") && fx.deferred("T").is_some(), "its change waits");
        assert!(applied.examine.contains(&(PathBuf::from("top.txt"), false)), "the examination decides: {:?}", applied.examine);
    }
    assert!(!fx.path("docs/f.txt").exists(), "a delete not examined yet is not undone");

    // The outbox forgets the object once OneDrive's change won: then it comes back.
    fx.store.with(|s| s.set_local_handle("T", None)).unwrap();
    fx.cycle(&[file("T", "R", "top.txt", "c2")], false).unwrap();
    assert_eq!(id_at(&fx.path("top.txt")).as_deref(), Some("T"), "changed in OneDrive: it comes back");
    assert_eq!(placeholder::read_state(&File::open(fx.path("top.txt")).unwrap()).unwrap(), Some(State::OnlineOnly));

    // F82 (8): an item the outbox forgot (its local object dropped) is placed again.
    fx.store.with(|s| s.set_local_handle("F", None)).unwrap();
    fx.cycle(&[], false).unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert!(fx.store.with(|s| s.local_handle("F")).unwrap().is_some(), "and its object recorded again");
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
    assert_eq!(applied.copies.len(), 1);
    assert!(applied.examine.contains(&(PathBuf::from("docs/new-fedora.txt"), false)));

    std::fs::write(fx.path("docs/other.txt"), b"mine too").unwrap();
    fx.row(OutboxKind::Create, None, "docs/other.txt");
    let applied = fx.cycle(&[file("O", "D", "other.txt", "c1")], false).unwrap();
    assert_eq!(std::fs::read(fx.path("docs/other.txt")).unwrap(), b"mine too", "the outbox settles create/create");
    assert!(applied.copies.is_empty() && applied.unsettled.contains("O"));
    assert!(fx.base("O").is_none() && fx.deferred("O").is_some(), "the new item waits");

    // A save by rename (the placeholder replaced by a new file): OneDrive
    // changed nothing, so the file stays as it is.
    std::fs::remove_file(fx.path("top.txt")).unwrap();
    std::fs::write(fx.path("top.txt"), b"saved").unwrap();
    let applied = fx.cycle(&[], true).unwrap();
    assert_eq!(std::fs::read(fx.path("top.txt")).unwrap(), b"saved");
    assert!(applied.copies.is_empty());

    // M3: a folder made here where a new one arrives from OneDrive is left
    // for the two to merge (the `mkdir`'s `409`), never copied aside.
    std::fs::create_dir(fx.path("photos")).unwrap();
    std::fs::write(fx.path("photos/mine.jpg"), b"jpg").unwrap();
    let applied = fx.cycle(&[folder("P", "R", "photos"), file("Q", "P", "theirs.jpg", "c1")], false).unwrap();
    assert!(applied.copies.is_empty() && !fx.path("photos-fedora").exists());
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
    assert_eq!(applied.copies[0].copy, PathBuf::from("docs/f-fedora.txt"));
    assert_eq!(fx.base("F").unwrap().ctag.as_deref(), Some("c2"));
}

/// §3.7: what OneDrive removed goes only where nothing local is lost — a
/// changed file stays, stripped, to be uploaded again; a folder that keeps
/// anything stays, made local, to be made again in OneDrive (F82 (4)); clean
/// placeholders go.
#[test]
fn what_onedrive_removed_goes_unless_it_holds_local_work() {
    let fx = Fx::new();
    fx.cycle(&[folder("X", "D", "empty")], false).unwrap();
    assert!(fx.path("docs/empty").is_dir());
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    edit(&fx.path("docs/f.txt"), b" and mine");
    std::fs::write(fx.path("docs/deep/mine.txt"), b"new here").unwrap();
    // A file from elsewhere — another account's, say — carrying an id the base does not know.
    std::fs::write(fx.path("docs/stranger.txt"), b"theirs").unwrap();
    xattr::set(fx.path("docs/stranger.txt"), XATTR_ITEM_ID, b"Y").unwrap();
    let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
    assert_eq!(std::fs::read(fx.path("docs/f.txt")).unwrap(), b"one and mine");
    assert_eq!(id_at(&fx.path("docs/f.txt")), None, "uploaded again as new");
    assert!(fx.path("docs/deep/mine.txt").exists());
    assert!(!fx.path("docs/deep/g.txt").exists(), "a placeholder holds nothing here");
    assert!(!fx.path("docs/empty").exists(), "an empty folder goes");
    assert_eq!(id_at(&fx.path("docs/stranger.txt")).as_deref(), Some("Y"), "never ours to remove");
    assert_eq!(id_at(&fx.path("docs")), None, "the folder stays, as a new one");
    assert_eq!(id_at(&fx.path("docs/deep")), None);
    let mut recreated = applied.recreated.clone();
    recreated.sort();
    assert_eq!(recreated, vec!["D".to_owned(), "E".to_owned()]);
    assert!(applied.examine.contains(&(PathBuf::from("docs"), true)));
    assert!(fx.base("D").is_none() && fx.base("F").is_none(), "gone from the base");
}

/// F82 (5): an item OneDrive has under the outbox's temporary name (a store
/// rebuilt before its final move) keeps its local object where it is: the
/// examination moves it back.
#[test]
fn an_item_under_a_temporary_name_in_onedrive_stays_where_it_is_here() {
    let fx = Fx::new();
    let swapped = Change::Upsert(Row { placement: Placement::Skipped(crate::tree::SkipReason::ReservedName), ..row("F", "D", ".konedrive-swap-F", Kind::File, "c1") });
    fx.cycle(&[swapped], true).unwrap();
    assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(fx.base("F").unwrap().name, ".konedrive-swap-F", "the base says where it is in OneDrive");
}

/// I2: a folder removed in OneDrive that holds nothing to upload — a clean
/// download merely open, a lock file an ignored name — is not made again in
/// OneDrive: it keeps its id and its base, and its removal waits until
/// nothing in it is in use. M7: an emptied download is local work.
#[test]
fn a_folder_removed_in_onedrive_waits_for_what_is_in_use_and_comes_back_only_for_local_work() {
    for write in [false, true] {
        let fx = Fx::new();
        hydrate(&fx.path("docs/f.txt"), b"one", "c1");
        std::fs::write(fx.path("docs/.~lock.f.txt#"), b"lock").unwrap();
        let open = std::fs::OpenOptions::new().read(true).write(write).open(fx.path("docs/f.txt")).unwrap();
        let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
        assert!(applied.recreated.is_empty(), "write={write}: {:?}", applied.recreated);
        assert_eq!(id_at(&fx.path("docs")).as_deref(), Some("D"), "write={write}: the folder keeps its id");
        assert_eq!(id_at(&fx.path("docs/f.txt")).as_deref(), Some("F"));
        assert!(!fx.path("docs/deep").exists(), "what could go went");
        assert!(fx.base("D").is_some() && fx.base("F").is_some(), "their base stays");
        assert!(fx.deferred("D").is_some() && fx.deferred("F").is_some(), "their removal waits");
        drop(open);
        std::fs::remove_file(fx.path("docs/.~lock.f.txt#")).unwrap();
        fx.cycle(&[Change::Delete("D".into())], false).unwrap();
        assert!(!fx.path("docs").exists(), "write={write}: gone once nothing is in use");
        assert!(fx.base("D").is_none());
    }

    // M7: an emptied download goes up again as new, with its folder.
    let fx = Fx::new();
    hydrate(&fx.path("docs/f.txt"), b"one", "c1");
    std::thread::sleep(std::time::Duration::from_millis(10));
    File::options().write(true).truncate(true).open(fx.path("docs/f.txt")).unwrap();
    let applied = fx.cycle(&[Change::Delete("D".into())], false).unwrap();
    assert_eq!(applied.recreated, vec!["D".to_owned()]);
    assert!(fx.path("docs/f.txt").exists() && id_at(&fx.path("docs/f.txt")).is_none());
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
        assert!(!fx.path("N").exists() && !fx.path("docs/N").exists(), "with_a_file={with_a_file}: {:?}", applied.examine);
        assert!(!fx.path("docs/.konedrive-new-N").exists() && !fx.path(".konedrive-holding").exists());
        assert!(applied.rescued.is_empty());
        assert_eq!(fx.path("docs/mine.txt").exists(), with_a_file, "what was in it is put back where it stood");
        assert!(applied.examine.iter().all(|(rel, _)| rel == Path::new("docs/mine.txt")), "{:?}", applied.examine);
    }
}
