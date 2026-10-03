use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{write_stamp, write_state, State, XATTR_ROOT};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use xattr::FileExt;

use super::*;
use crate::sync::root::SyncRoot;
use konedrive_tree::{Change, TreeStore};

struct Fixture {
    _dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    rescue: tempfile::TempDir,
    /// `None` in an async test: the `Materializer`'s handle then comes
    /// from `Handle::current()`, since there is already a runtime here.
    runtime: Option<tokio::runtime::Runtime>,
}

fn build_fixture(runtime: Option<tokio::runtime::Runtime>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let root_id = "8f6c0a3e-3b0e-4d7a-9c1e-5b2d7e4f1a90".to_owned();
    File::open(&path).unwrap().set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
    Fixture {
        _dir: dir,
        root: SyncRoot { path, root_id },
        store: Store::new(TreeStore::in_memory().unwrap()),
        rescue: tempfile::tempdir().unwrap(),
        runtime,
    }
}

fn fixture() -> Fixture {
    build_fixture(Some(tokio::runtime::Runtime::new().unwrap()))
}

/// A fixture for an already-`async` test: no `Runtime` of its own.
fn fixture_async() -> Fixture {
    build_fixture(None)
}

fn row(id: &str, parent: &str, name: &str, kind: Kind, size: u64) -> Row {
    Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind, size, mtime: 1_700_000_000, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed }
}

fn root_row() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

fn up(row: Row) -> Change {
    Change::Upsert(row)
}

fn folder(id: &str, parent: &str, name: &str) -> Change {
    up(row(id, parent, name, Kind::Folder, 0))
}

fn file(id: &str, parent: &str, name: &str) -> Change {
    up(row(id, parent, name, Kind::File, 4096))
}

/// A locked tree cannot be deleted by the temporary directory's cleanup.
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(disk) = Disk::open(&self.root, false) {
            let _ = disk.unlock_tree();
        }
    }
}

impl Fixture {
    /// The handle a `Materializer` waits on the helper through: this
    /// fixture's own runtime, or — in an async test, which has none of
    /// its own — the one already running it.
    fn handle(&self) -> tokio::runtime::Handle {
        match &self.runtime {
            Some(rt) => rt.handle().clone(),
            None => tokio::runtime::Handle::current(),
        }
    }

    fn materializer(&self, locked: bool, link: Option<HelperLink>) -> Materializer {
        Materializer {
            disk: Disk::open(&self.root, locked).unwrap(),
            store: self.store.clone(),
            link,
            runtime: self.handle(),
            locks: InodeLocks::new(),
            root_item_id: "R".into(),
            rescue_into: self.rescue.path().join("now"),
            cancel: CancellationToken::new(),
            rw: None,
            claimed: None,
        }
    }

    /// A full listing of `changes`, reconciled and committed.
    fn listed(&self, changes: &[Change], locked: bool) -> Applied {
        { let changes = changes.to_vec(); self.store.call_blocking(move |s| { s.begin_staging(false)?; s.stage(&changes) }).unwrap(); }
        let applied = self.materializer(locked, None).apply(Scope::Full).unwrap();
        self.store.call_blocking(move |s| s.commit_staging("link-1")).unwrap();
        applied
    }

    /// A delta on top of what is committed, reconciled in the Changed scope.
    fn delta(&self, changes: &[Change], locked: bool) -> Result<Applied, ApplyError> {
        { let changes = changes.to_vec(); self.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&changes) }).unwrap(); }
        let ids = changes.iter().map(|c| c.id().to_owned()).collect();
        self.materializer(locked, None).apply(Scope::Changed(ids))
    }

    /// [`Self::listed`] with `tree()`, from an async test: the reconcile
    /// itself runs on a blocking thread, since it may wait on the
    /// runtime it is itself running on (`Materializer::mark`).
    async fn listed_async(&self, locked: bool) -> Applied {
        self.store.call(move |s| { s.begin_staging(false)?; s.stage(&tree()) }).await.unwrap();
        let m = self.materializer(locked, None);
        let applied = tokio::task::spawn_blocking(move || m.apply(Scope::Full)).await.unwrap().unwrap();
        self.store.call(move |s| s.commit_staging("link-1")).await.unwrap();
        applied
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }
}

fn id_at(path: &Path) -> Option<String> {
    xattr::get(path, "user.konedrive.item-id").unwrap().map(|v| String::from_utf8(v).unwrap())
}

fn ino(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).unwrap().ino()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn tree() -> Vec<Change> {
    vec![root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt"), folder("E", "D", "deep"), file("G", "E", "g.txt"), file("T", "R", "top.bin")]
}

/// Round 2: `(dev, ino)` alone is not a strong enough identity for a file
/// that was dropped and reopened later — an inode can be freed and
/// reused by an unrelated file in between. `FileIdentity` must tell that
/// case apart, which an ino-only comparison cannot.
#[test]
fn a_reused_inode_with_a_different_birth_time_is_a_different_file() {
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500);
    let a = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
    let same = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
    let reused = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t2) };
    assert_eq!(a, same, "the same dev, ino and birth time is the same file");
    assert_ne!(a, reused, "the same ino with a different birth time is a different file");
}

/// Issue #104, decisions 4 and 5, read-only: what the reconcile takes
/// off the disk — here a folder that is no longer placed (a name too
/// long) — is forgotten in the store before it goes, and a download into
/// a file in it stops; placed again, it records its new objects.
#[test]
fn a_read_only_removal_forgets_first_and_stops_a_download() {
    let fx = fixture();
    fx.listed(&tree(), false);
    let handle = |id: &str| { let id = id.to_owned(); fx.store.call_blocking(move |s| s.local_handle(&id)).unwrap() };
    assert!(handle("E").is_some() && handle("G").is_some());
    let locks = InodeLocks::new();
    let file = File::open(fx.path("docs/deep/g.txt")).unwrap();
    let rt = fx.runtime.as_ref().unwrap();
    let guard = rt.block_on(locks.lock(crate::sync::InodeKey::of(&file).unwrap()));
    let skipped = up(Row { placement: Placement::Skipped(konedrive_tree::SkipReason::NameTooLong), ..row("E", "D", &"x".repeat(300), Kind::Folder, 0) });
    { let changes = vec![skipped.clone()]; fx.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&changes) }).unwrap(); }
    Materializer { locks: locks.clone(), ..fx.materializer(false, None) }.apply(Scope::Changed(vec!["E".into()])).unwrap();
    assert_eq!((handle("E"), handle("G")), (None, None), "forgotten before the swap");
    fx.store.call_blocking(move |s| s.commit_staging("link-2")).unwrap();
    assert!(!fx.path("docs/deep").exists());
    assert_eq!((handle("E"), handle("G")), (None, None), "and after it");
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), guard.cancelled()).await }).expect("the download was told to stop");
    drop(guard);

    fx.delta(&[folder("E", "D", "deep")], false).unwrap();
    fx.store.call_blocking(move |s| s.commit_staging("link-3")).unwrap();
    let placed = konedrive_fs::handle::FileHandle::of(&File::open(fx.path("docs/deep/g.txt")).unwrap()).unwrap();
    assert_eq!(handle("G"), Some(placed), "placed again, with its new object");
}

/// Review fix 6 of issue #104, read-only: a file of another account being
/// downloaded inside a folder removed in OneDrive is set aside alive, as
/// always — but its download is stopped first and it is a placeholder
/// again, not partly filled.
#[test]
fn a_stopped_download_set_aside_for_another_account_is_a_placeholder_again() {
    let fx = fixture();
    fx.listed(&tree(), false);
    let at = fx.path("docs/theirs.bin");
    std::fs::write(&at, vec![7u8; 8192]).unwrap();
    let file = File::open(&at).unwrap();
    placeholder::write_item_id(&file, "Y").unwrap();
    placeholder::write_state(&file, State::Hydrating).unwrap();
    let key = crate::sync::InodeKey::of(&file).unwrap();
    drop(file);
    let locks = InodeLocks::new();
    let rt = fx.runtime.as_ref().unwrap();
    let (held, holding) = std::sync::mpsc::channel();
    let fill = rt.spawn({
        let locks = locks.clone();
        async move {
            let guard = locks.lock(key).await;
            held.send(()).unwrap();
            guard.cancelled().await;
        }
    });
    holding.recv().unwrap();
    let claimed: Claimed = std::sync::Arc::new(|id: &str| id == "Y");
    { fx.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[Change::Delete("D".into())]) }).unwrap(); }
    let applied = Materializer { locks: locks.clone(), claimed: Some(claimed), ..fx.materializer(false, None) }.apply(Scope::Changed(vec!["D".into()])).unwrap();
    rt.block_on(fill).unwrap();
    let aside = applied.rescued.iter().find(|r| r.original.ends_with("theirs.bin")).expect("set aside").rescued.clone();
    let file = File::open(&aside).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly), "a placeholder again");
    assert_eq!(file.metadata().unwrap().blocks(), 0, "with nothing of the stopped download in it");
    assert_eq!(placeholder::read_item_id(&file).unwrap().as_deref(), Some("Y"), "still the other account's");
}

use std::os::unix::fs::FileExt as _;

use async_trait::async_trait;
use konedrive_fs::placeholder::{read_ctag, read_progress, write_ctag, write_progress, Progress};

use konedrive_graph::quickxor::QuickXor;
use crate::sync::source::{ContentSource, Fetched, SourceError, Version};

/// Downloads a file the way a finished fill leaves it: content, cTag, stamp.
fn hydrate_by_hand(path: &Path, content: &[u8], ctag: &str) {
    let file = konedrive_fs::placeholder::reopen_writable(&File::open(path).unwrap()).unwrap();
    file.set_len(0).unwrap();
    file.write_all_at(content, 0).unwrap();
    write_ctag(&file, ctag).unwrap();
    write_state(&file, State::Hydrated).unwrap();
    write_stamp(&file).unwrap();
}

fn changed(id: &str, parent: &str, name: &str, size: u64, ctag: &str) -> Change {
    let mut r = row(id, parent, name, Kind::File, size);
    r.ctag = Some(ctag.into());
    r.mtime = 1_700_000_500;
    up(r)
}

/// a new folder's temporary directory left with
/// no id — killed between `mkdirat` and its label — made every later
/// reconcile fail `EEXIST`, for good. An empty one is cleared and the
/// folder made; one with something in it is rescued first.
#[test]
fn a_new_folders_temporary_directory_left_without_its_id_is_cleared() {
    let f = fixture();
    std::fs::create_dir(f.path(".konedrive-new-D")).unwrap();
    f.listed(&[root_row(), folder("D", "R", "docs")], true);
    assert_eq!(id_at(&f.path("docs")).as_deref(), Some("D"));
    assert!(!f.path(".konedrive-new-D").exists());

    let docs = File::open(f.path("docs")).unwrap();
    placeholder::with_owner_write(&docs, || std::fs::create_dir(f.path("docs/.konedrive-new-E"))).unwrap();
    std::fs::write(f.path("docs/.konedrive-new-E/mine.txt"), b"mine").unwrap();
    let applied = f.delta(&[folder("E", "D", "deep")], true).unwrap();
    assert_eq!(id_at(&f.path("docs/deep")).as_deref(), Some("E"));
    assert_eq!(applied.rescued.len(), 1, "{:?}", applied.rescued);
    assert_eq!(std::fs::read(applied.rescued[0].rescued.join("mine.txt")).unwrap(), b"mine");
}

/// a directory of the user's own in the way,
/// holding a placeholder of ours, is rescued with the user's files — and
/// without the placeholder, which stripped of its state read as a file
/// of zeros in the rescue directory.
#[test]
fn a_rescued_directory_keeps_the_users_files_and_not_our_placeholders() {
    let f = fixture();
    f.listed(&[root_row()], false);
    std::fs::create_dir(f.path("docs")).unwrap();
    std::fs::write(f.path("docs/mine.txt"), b"mine").unwrap();
    let docs = File::open(f.path("docs")).unwrap();
    placeholder::create_placeholder(&docs, "cloud.bin", "X", 4096, SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();

    let applied = f.delta(&[folder("D", "R", "docs")], false).unwrap();

    let rescued = &applied.rescued[0].rescued;
    assert_eq!(std::fs::read(rescued.join("mine.txt")).unwrap(), b"mine");
    assert!(!rescued.join("cloud.bin").exists(), "a placeholder would read as zeros there");
    assert_eq!(id_at(&f.path("docs")).as_deref(), Some("D"));
}

#[test]
fn a_placeholder_changed_in_the_cloud_is_updated_in_place() {
    let f = fixture();
    f.listed(&tree(), true);
    let path = f.path("docs/f.txt");
    let before = ino(&path);
    let applied = f.delta(&[changed("F", "D", "f.txt", 8192, "c2")], true).unwrap();
    assert_eq!(applied.updated, 1);
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!((meta.ino(), meta.len(), meta.mtime()), (before, 8192, 1_700_000_500));
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    assert_eq!(mode(&path), 0o444);
}

#[test]
fn a_checkpoint_of_the_old_version_goes_with_it() {
    let f = fixture();
    f.listed(&tree(), false);
    let path = f.path("docs/f.txt");
    {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        file.write_all_at(&[5u8; 2048], 0).unwrap();
        write_progress(&file, &Progress { ctag: "c-F".into(), bytes: 2048 }).unwrap();
    }
    f.delta(&[changed("F", "D", "f.txt", 4096, "c2")], false).unwrap();
    let file = File::open(&path).unwrap();
    assert_eq!(read_progress(&file).unwrap(), None);
    assert!(std::fs::read(&path).unwrap().iter().all(|b| *b == 0), "the old version's bytes are gone");
}

#[test]
fn a_placeholder_emptied_in_the_cloud_becomes_an_empty_downloaded_file() {
    let f = fixture();
    f.listed(&tree(), false);
    f.delta(&[changed("F", "D", "f.txt", 0, "c2")], false).unwrap();
    let file = File::open(f.path("docs/f.txt")).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert!(stamp_matches(&file).unwrap());
}

#[test]
fn a_downloaded_file_of_the_same_version_is_left_alone() {
    let f = fixture();
    f.listed(&tree(), false);
    hydrate_by_hand(&f.path("docs/f.txt"), b"content", "c-F");
    let mut same = row("F", "D", "f.txt", Kind::File, 7);
    same.mtime = 1_700_000_900; // metadata changed, content did not
    let applied = f.delta(&[up(same)], false).unwrap();
    assert!(applied.replacements.is_empty());
    assert_eq!(std::fs::read(f.path("docs/f.txt")).unwrap(), b"content");
}

#[test]
fn a_downloaded_file_changed_in_the_cloud_is_queued_for_replacement() {
    let f = fixture();
    f.listed(&tree(), false);
    hydrate_by_hand(&f.path("docs/f.txt"), b"content", "c-F");
    let applied = f.delta(&[changed("F", "D", "f.txt", 9, "c2")], false).unwrap();
    assert_eq!(applied.replacements, vec![Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 9 }]);
    assert_eq!(std::fs::read(f.path("docs/f.txt")).unwrap(), b"content", "untouched until replaced");
}

#[test]
fn a_file_changed_here_and_in_the_cloud_is_rescued_and_shown_as_the_new_version() {
    let f = fixture();
    f.listed(&tree(), false);
    let path = f.path("docs/f.txt");
    hydrate_by_hand(&path, b"content", "c-F");
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all_at(b" and mine", 7).unwrap();
    let applied = f.delta(&[changed("F", "D", "f.txt", 9, "c2")], false).unwrap();
    assert_eq!(applied.rescued.len(), 1);
    assert_eq!(applied.rescued[0].original, PathBuf::from("docs/f.txt"), "where it was, for the conflict");
    assert_eq!(std::fs::read(&applied.rescued[0].rescued).unwrap(), b"content and mine");
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
}

/// An incremental cycle says what it did item by item —
/// added, updated, moved (and from where), removed — and a folder removed
/// with everything in it is one removal, not one per file.
#[test]
fn a_changed_scope_notes_each_item_it_changed() {
    let f = fixture();
    f.listed(&tree(), false);
    let applied = f
        .delta(
            &[
                file("N", "R", "new.txt"),
                changed("F", "D", "f.txt", 9, "c2"),
                file("T", "R", "renamed.bin"),
                Change::Delete("E".into()),
            ],
            false,
        )
        .unwrap();
    let mut changes = applied.changes.clone();
    changes.sort_by(|a, b| a.rel.cmp(&b.rel));
    let change = |kind, rel: &str, from: Option<&str>| Changed { kind, rel: rel.into(), from: from.map(PathBuf::from) };
    assert_eq!(
        changes,
        vec![
            change(EventKind::Removed, "docs/deep", None),
            change(EventKind::Updated, "docs/f.txt", None),
            change(EventKind::Added, "new.txt", None),
            change(EventKind::Moved, "renamed.bin", Some("top.bin")),
        ]
    );
}

#[test]
fn a_file_being_filled_is_left_for_the_next_cycle() {
    let f = fixture();
    f.listed(&tree(), false);
    let path = f.path("docs/f.txt");
    let m = f.materializer(false, None);
    let _held = m.locks.try_lock(crate::sync::InodeKey::of(&File::open(&path).unwrap()).unwrap()).unwrap();
    f.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[changed("F", "D", "f.txt", 8192, "c2")]) }).unwrap();
    let applied = m.apply(Scope::Changed(vec!["F".into()])).unwrap();
    assert_eq!((applied.updated, applied.deferred), (0, 1));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
}

/// Serves `content` as version `ctag`, with its hash; `on_fetch` runs first.
/// `damaged` flips one byte of what it streams, not of what it hashes.
struct Memory {
    ctag: String,
    content: Vec<u8>,
    damaged: bool,
    on_fetch: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Memory {
    fn new(ctag: &str, content: &[u8]) -> Self {
        Self { ctag: ctag.into(), content: content.to_vec(), damaged: false, on_fetch: std::sync::Mutex::new(None) }
    }
}

#[async_trait]
impl ContentSource for Memory {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        if let Some(hook) = self.on_fetch.lock().unwrap().take() {
            hook();
        }
        let mut hash = QuickXor::new();
        hash.update(&self.content);
        let mut served = self.content.clone();
        if self.damaged {
            served[0] ^= 1;
        }
        let start = (from as usize).min(served.len());
        Ok(Fetched {
            served_from: from,
            size: self.content.len() as u64,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500),
            version: Some(Version { ctag: self.ctag.clone(), quick_xor: Some(hash.finish()) }),
            stream: Box::new(std::io::Cursor::new(served[start..].to_vec())),
        })
    }
}

#[tokio::test]
async fn a_replacement_swaps_in_the_new_version_and_a_reader_keeps_the_old() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, true).unwrap(), f.path("docs/f.txt"));
    f.listed_async(true).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let reader = File::open(&path).unwrap();
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &Memory::new("c2", b"the new version"), &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"the new version");
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    assert!(stamp_matches(&file).unwrap());
    assert_eq!(mode(&path), 0o444);
    let mut old = vec![0u8; 11];
    reader.read_exact_at(&mut old, 0).unwrap();
    assert_eq!(&old, b"old version", "a reader of the old file keeps it");
}

#[tokio::test]
async fn a_replacement_that_does_not_match_its_hash_leaves_the_old_version() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let damaged = Memory { damaged: true, ..Memory::new("c2", b"the new version") };
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &damaged, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
    assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
}

#[tokio::test]
async fn a_replacement_that_cannot_be_downloaded_leaves_the_old_version() {
    struct Gone;
    #[async_trait]
    impl ContentSource for Gone {
        async fn fetch(&self, _: &str, _: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
            Err(SourceError::NotFound("gone".into()))
        }
    }
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &Gone, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
    assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
}

#[tokio::test]
async fn a_file_freed_up_while_its_replacement_downloaded_is_left_as_it_is() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let source = Memory::new("c2", b"the new version");
    let freed = path.clone();
    *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
        let file = File::options().read(true).write(true).open(&freed).unwrap();
        // `old` must not still be open here, or Free up
        // space could not take this file's write lease while its
        // replacement downloads.
        assert!(
            konedrive_fs::lease::WriteLease::take(&file).unwrap().is_some(),
            "the old file's write lease is free while its replacement downloads"
        );
        write_state(&file, State::OnlineOnly).unwrap();
    }));
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
    assert_eq!(ino(&path), before, "the user freed it up; it is not filled behind their back");
}

/// A folder above the file moves (or is removed) while
/// its replacement downloads. `disk.dir(parent)` then answers ENOENT —
/// the same "nothing to do any more, the next cycle looks again" case as
/// any other change underneath the replacement, not a download failure to
/// report and keep retrying forever.
#[tokio::test]
async fn a_folder_moved_while_its_replacement_downloaded_is_left_as_it_is() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let source = Memory::new("c2", b"the new version");
    let root = f.root.path.clone();
    *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
        std::fs::rename(root.join("docs"), root.join("papers")).unwrap();
    }));
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
    assert_eq!(ino(&f.path("papers/f.txt")), before, "the file is untouched at its new path");
    assert_eq!(std::fs::read(f.path("papers/f.txt")).unwrap(), b"old version");
}

/// The swap under the old file's lock really does
/// wait for it — untested until now — rather than racing whoever holds
/// it (a fill, a Free up, another replacement of the same file).
#[tokio::test]
async fn a_replacements_swap_waits_for_the_per_inode_lock() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let locks = InodeLocks::new();
    let key = crate::sync::InodeKey::of(&File::open(&path).unwrap()).unwrap();
    let held = locks.lock(key).await;

    let task_locks = locks.clone();
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let handle = tokio::spawn(async move {
        let source = Memory::new("c2", b"the new version");
        replace(&disk, &task_locks, &source, &replacement).await
    });

    // The download itself is instant (an in-memory source, no delay);
    // this is time enough for the task to reach the lock and block on it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!handle.is_finished(), "the swap has not gone ahead while the lock is held");
    assert_eq!(std::fs::read(&path).unwrap(), b"old version", "not swapped in yet");

    drop(held);
    let outcome = handle.await.unwrap();
    assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"the new version", "swapped in once the lock is free");
}

/// A file mid-fill (`Hydrating`) is left exactly
/// alone by `check_file` — untested until now — deferred like a file
/// being freed up, never touched.
#[test]
fn a_file_hydrating_right_now_is_left_for_the_next_cycle() {
    let f = fixture();
    f.listed(&tree(), false);
    let path = f.path("docs/f.txt");
    {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        write_state(&file, State::Hydrating).unwrap();
    }
    let applied = f.delta(&[changed("F", "D", "f.txt", 8192, "c2")], false).unwrap();
    assert_eq!((applied.updated, applied.deferred), (0, 1));
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrating));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096, "untouched");
}

#[test]
fn a_full_reconcile_builds_the_tree_from_nothing() {
    let f = fixture();
    let applied = f.listed(&tree(), false);
    assert_eq!(applied.created, 5);
    for (rel, id) in [("docs", "D"), ("docs/f.txt", "F"), ("docs/deep", "E"), ("docs/deep/g.txt", "G"), ("top.bin", "T")] {
        assert_eq!(id_at(&f.path(rel)).as_deref(), Some(id), "{rel}");
    }
    let meta = std::fs::metadata(f.path("docs/f.txt")).unwrap();
    assert_eq!((meta.len(), meta.mtime()), (4096, 1_700_000_000));
    assert!(!f.path(".konedrive-holding").exists());
    assert_eq!(mode(&f.path("docs/f.txt")), 0o644, "no lock on an unlocked folder");
}

#[test]
fn under_the_lock_everything_ends_read_only() {
    let f = fixture();
    f.listed(&tree(), true);
    for rel in ["", "docs", "docs/deep"] {
        assert_eq!(mode(&f.path(rel)), 0o555, "{rel:?}");
    }
    for rel in ["docs/f.txt", "top.bin", "docs/deep/g.txt"] {
        assert_eq!(mode(&f.path(rel)), 0o444, "{rel}");
    }
    let refused = std::fs::write(f.path("docs/new.txt"), b"x").unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn a_rename_keeps_the_inode() {
    let f = fixture();
    f.listed(&tree(), true);
    let before = ino(&f.path("docs/f.txt"));
    f.delta(&[file("F", "D", "renamed.txt")], true).unwrap();
    assert_eq!(ino(&f.path("docs/renamed.txt")), before);
    assert!(!f.path("docs/f.txt").exists());
}

#[test]
fn a_moved_folder_takes_its_contents_along() {
    let f = fixture();
    f.listed(&tree(), true);
    let before = ino(&f.path("docs/deep/g.txt"));
    f.delta(&[folder("E", "R", "moved")], true).unwrap();
    assert_eq!(ino(&f.path("moved/g.txt")), before);
    assert_eq!(id_at(&f.path("moved")).as_deref(), Some("E"));
    assert!(!f.path("docs/deep").exists());
    assert_eq!(mode(&f.path("moved")), 0o555);
}

/// Phase 1 goes deepest first: when a folder and something inside it both
/// move, the inner one leaves before the folder changes its path.
#[test]
fn a_folder_and_a_file_inside_it_move_in_one_delta() {
    let f = fixture();
    f.listed(&tree(), true);
    let before = ino(&f.path("docs/deep/g.txt"));
    f.delta(&[folder("E", "R", "moved"), file("G", "R", "g.txt")], true).unwrap();
    assert_eq!(ino(&f.path("g.txt")), before);
    assert_eq!(id_at(&f.path("moved")).as_deref(), Some("E"));
    assert!(!f.path("docs/deep").exists());
}

/// The same order in a Full reconcile, which finds the misplaced items by
/// scanning.
#[test]
fn a_full_reconcile_moves_the_inner_of_two_misplaced_items_first() {
    let f = fixture();
    f.listed(&tree(), false);
    let before = ino(&f.path("docs/deep/g.txt"));
    f.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[folder("E", "R", "deep"), file("G", "E", "g2.txt")]) }).unwrap();
    f.materializer(false, None).apply(Scope::Full).unwrap();
    assert_eq!(ino(&f.path("deep/g2.txt")), before);
    assert!(!f.path("docs/deep").exists());
}

#[test]
fn two_names_swapped_end_up_swapped() {
    let f = fixture();
    f.listed(&[root_row(), file("A", "R", "a"), file("B", "R", "b")], true);
    let (a, b) = (ino(&f.path("a")), ino(&f.path("b")));
    let applied = f.delta(&[file("A", "R", "b"), file("B", "R", "a")], true).unwrap();
    assert_eq!((ino(&f.path("b")), ino(&f.path("a"))), (a, b));
    assert!(applied.rescued.is_empty());
    assert!(!f.path(".konedrive-holding").exists());
}

#[test]
fn a_cycle_of_three_resolves() {
    let f = fixture();
    f.listed(&[root_row(), file("A", "R", "a"), file("B", "R", "b"), file("C", "R", "c")], false);
    let (a, b, c) = (ino(&f.path("a")), ino(&f.path("b")), ino(&f.path("c")));
    f.delta(&[file("A", "R", "b"), file("B", "R", "c"), file("C", "R", "a")], false).unwrap();
    assert_eq!((ino(&f.path("b")), ino(&f.path("c")), ino(&f.path("a"))), (a, b, c));
}

#[test]
fn a_deleted_folder_goes_but_a_file_changed_here_is_rescued() {
    let f = fixture();
    f.listed(&tree(), true);
    // f.txt was downloaded, then written to through a descriptor someone
    // opened during a lock window.
    let path = f.path("docs/f.txt");
    {
        let file = konedrive_fs::placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&file, &[9u8; 4096], 0).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();
        // Appended, so the size changes: an mtime can land in the same
        // clock tick as the stamp.
        std::os::unix::fs::FileExt::write_all_at(&file, b"local work", 4096).unwrap();
    }
    let applied = f.delta(&[Change::Delete("D".into())], true).unwrap();
    assert!(!f.path("docs").exists());
    assert_eq!(
        applied.rescued,
        vec![Rescued { original: "docs/f.txt".into(), rescued: f.rescue.path().join("now/docs/f.txt") }]
    );
    let kept = &applied.rescued[0].rescued;
    assert!(std::fs::read(kept).unwrap().ends_with(b"local work"));
    assert_eq!(mode(kept), 0o644);
    let names: Vec<_> = xattr::list(kept).unwrap().collect();
    assert!(names.iter().all(|n| !n.to_string_lossy().starts_with("user.konedrive.")), "{names:?}");
}

#[test]
fn a_name_too_long_is_not_created_and_a_folder_renamed_to_one_leaves() {
    let f = fixture();
    let long = "я".repeat(128);
    let mut skipped = row("L", "R", &long, Kind::File, 1);
    skipped.placement = Placement::Skipped(konedrive_tree::SkipReason::NameTooLong);
    f.listed(&[root_row(), up(skipped), folder("D", "R", "docs"), file("F", "D", "f.txt")], true);
    assert_eq!(std::fs::read_dir(&f.root.path).unwrap().count(), 1, "only docs");
    let mut renamed = row("D", "R", &long, Kind::Folder, 0);
    renamed.placement = Placement::Skipped(konedrive_tree::SkipReason::NameTooLong);
    f.delta(&[up(renamed)], true).unwrap();
    assert!(!f.path("docs").exists(), "a folder that can no longer be shown is removed; its clean files are in the cloud");
}

#[test]
fn the_order_of_the_rows_does_not_matter() {
    let f = fixture();
    let mut changes = tree();
    changes.reverse();
    f.listed(&changes, false);
    assert_eq!(id_at(&f.path("docs/deep/g.txt")).as_deref(), Some("G"));
}

#[test]
fn a_folder_that_does_not_match_the_stored_tree_needs_a_full_reconcile() {
    let f = fixture();
    f.listed(&tree(), false);
    std::fs::remove_file(f.path("docs/f.txt")).unwrap();
    let err = f.delta(&[file("F", "D", "renamed.txt")], false).unwrap_err();
    assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
    f.materializer(false, None).apply(Scope::Full).unwrap();
    assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
}

/// A stranger directory where a new folder belongs leaves whole, by one
/// rename, even locked and read-only itself; it arrives as the user's own.
#[test]
fn a_stranger_folder_in_the_way_is_rescued_whole_under_the_lock() {
    let f = fixture();
    f.listed(&tree(), true);
    std::fs::set_permissions(&f.root.path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(f.path("incoming")).unwrap();
    std::fs::write(f.path("incoming/mine.txt"), b"mine").unwrap();
    std::fs::set_permissions(f.path("incoming"), std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::set_permissions(&f.root.path, std::fs::Permissions::from_mode(0o555)).unwrap();
    let applied = f.delta(&[folder("N", "R", "incoming")], true).unwrap();
    assert_eq!(applied.rescued, vec![Rescued { original: "incoming".into(), rescued: f.rescue.path().join("now/incoming") }]);
    assert_eq!(std::fs::read(f.rescue.path().join("now/incoming/mine.txt")).unwrap(), b"mine");
    assert_eq!(mode(&f.rescue.path().join("now/incoming")), 0o755);
    assert_eq!(id_at(&f.path("incoming")).as_deref(), Some("N"));
    assert_eq!(mode(&f.root.path), 0o555);
}

/// The Changed scope puts an item only into a folder it has checked is
/// ours: a folder swapped for a stranger of the same name (the lock
/// bypassed) hands over to a Full reconcile, and nothing is made in it.
#[test]
fn a_changed_delta_does_not_place_into_a_folder_that_is_not_ours() {
    let f = fixture();
    f.listed(&tree(), false);
    std::fs::rename(f.path("docs/deep"), f.path("elsewhere")).unwrap();
    std::fs::create_dir(f.path("docs/deep")).unwrap();
    let err = f.delta(&[file("N", "E", "new.txt")], false).unwrap_err();
    assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
    assert!(!f.path("docs/deep/new.txt").exists());
}

/// A Changed run that finds a holding directory left by an earlier run
/// hands over to a Full reconcile rather than drain what it did not put
/// there — here a file still in the tree.
#[test]
fn a_changed_run_that_finds_a_holding_directory_needs_a_full_reconcile() {
    let f = fixture();
    f.listed(&tree(), false);
    std::fs::create_dir(f.path(".konedrive-holding")).unwrap();
    std::fs::rename(f.path("top.bin"), f.path(".konedrive-holding/T")).unwrap();
    let err = f.delta(&[file("F", "D", "renamed.txt")], false).unwrap_err();
    assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
    assert_eq!(id_at(&f.path(".konedrive-holding/T")).as_deref(), Some("T"), "nothing drained");
    f.materializer(false, None).apply(Scope::Full).unwrap();
    assert_eq!(id_at(&f.path("top.bin")).as_deref(), Some("T"));
    assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!f.path(".konedrive-holding").exists());
}

#[test]
fn a_full_reconcile_repairs_whatever_it_finds() {
    let f = fixture();
    f.listed(&tree(), false);
    // A file of ours in the wrong folder, a stranger where a new item
    // belongs, and a folder a crash left under its temporary name.
    std::fs::rename(f.path("docs/f.txt"), f.path("f-in-the-wrong-place")).unwrap();
    std::fs::write(f.path("docs/new.txt"), b"mine").unwrap();
    std::fs::rename(f.path("docs/deep"), f.path("docs/.konedrive-new-E")).unwrap();
    f.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[file("N", "D", "new.txt")]) }).unwrap();
    let applied = f.materializer(false, None).apply(Scope::Full).unwrap();
    assert_eq!(id_at(&f.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(id_at(&f.path("docs/deep")).as_deref(), Some("E"));
    assert_eq!(id_at(&f.path("docs/new.txt")).as_deref(), Some("N"));
    assert_eq!(
        applied.rescued,
        vec![Rescued { original: "docs/new.txt".into(), rescued: f.rescue.path().join("now/docs/new.txt") }]
    );
    assert_eq!(std::fs::read(&applied.rescued[0].rescued).unwrap(), b"mine");
    assert!(applied.changes.is_empty(), "a Full reconcile is one listed event, not one per item");
    assert!(!f.path("f-in-the-wrong-place").exists());
}

/// What `swap_in` leaves when a crash — or a rename
/// that fails after its `linkat` succeeded — lands before the file it
/// downloaded ever lands on the old name: a second name for the same
/// item id, carrying the new version, that a Full reconcile must not try
/// to send to holding alongside the real (still current) file.
#[test]
fn a_replacement_link_left_by_a_crashed_swap_is_discarded_and_the_real_file_still_moves() {
    let f = fixture();
    f.listed(&tree(), false);
    let disk = Disk::open(&f.root, false).unwrap();
    let dir = disk.dir(Path::new("docs")).unwrap();
    let leftover = disk.tmpfile(&dir).unwrap();
    placeholder::write_item_id(&leftover, "F").unwrap();
    leftover.write_all_at(b"the new version", 0).unwrap();
    placeholder::write_ctag(&leftover, "c2").unwrap();
    placeholder::write_state(&leftover, State::Hydrated).unwrap();
    placeholder::write_stamp(&leftover).unwrap();
    nix::unistd::linkat(leftover.as_fd(), "", dir.as_fd(), OsStr::new(".konedrive-new-F"), nix::fcntl::AtFlags::AT_EMPTY_PATH).unwrap();
    drop(leftover);
    assert!(f.path("docs/.konedrive-new-F").exists());

    f.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[file("F", "D", "renamed.txt")]) }).unwrap();
    let applied = f.materializer(false, None).apply(Scope::Full).unwrap();
    assert!(!f.path("docs/.konedrive-new-F").exists(), "the leftover is gone");
    assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!f.path("docs/f.txt").exists());
    assert!(applied.rescued.is_empty(), "nothing here held local work");
}

#[test]
fn a_cancelled_reconcile_stops() {
    let f = fixture();
    f.store.call_blocking(move |s| { s.begin_staging(false)?; s.stage(&tree()) }).unwrap();
    let m = f.materializer(false, None);
    m.cancel.cancel();
    assert!(matches!(m.apply(Scope::Full), Err(ApplyError::Cancelled)));
}

/// Invariant M1: a new folder is marked before anything is created in it.
#[test]
fn a_new_folder_is_marked_while_it_is_still_empty() {
    let f = fixture();
    let sockets = tempfile::tempdir().unwrap();
    let socket = sockets.path().join("helper.sock");
    let marks = marking_helper(socket.clone());
    let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
    f.store.call_blocking(move |s| { s.begin_staging(false)?; s.stage(&tree()) }).unwrap();
    f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
    let seen: Vec<Marked> = marks.try_iter().collect();
    assert_eq!(seen.len(), 2, "docs and docs/deep were marked");
    assert!(seen.iter().all(|m| m.entries == 0), "entries at the time of marking: {seen:?}");
    let mut names: Vec<&str> = seen.iter().map(|m| m.name.as_str()).collect();
    names.sort();
    assert_eq!(names, [".konedrive-new-D", ".konedrive-new-E"], "marked before the real name shows the folder");
}

/// Invariant M1 across a failure: a folder whose marking failed is left
/// under its temporary name, and the Full reconcile that later places it
/// marks it before its real name shows it and before anything is put in it.
#[test]
fn a_folder_whose_marking_failed_is_marked_when_it_is_placed_later() {
    let f = fixture();
    let sockets = tempfile::tempdir().unwrap();
    let refusing = sockets.path().join("refusing.sock");
    let _refused = helper_answering(refusing.clone(), libc::EIO);
    let link = f.handle().block_on(HelperLink::connect(&refusing)).unwrap().0;
    f.store.call_blocking(move |s| { s.begin_staging(false)?; s.stage(&[root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt")]) }).unwrap();
    let err = f.materializer(true, Some(link)).apply(Scope::Full).unwrap_err();
    assert!(matches!(err, ApplyError::Mark(..)), "{err:?}");
    let unmarked = ino(&f.path(".konedrive-new-D"));

    let socket = sockets.path().join("helper.sock");
    let marks = marking_helper(socket.clone());
    let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
    f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
    assert_eq!(ino(&f.path("docs")), unmarked, "the folder made the first time is the one placed");
    assert_eq!(id_at(&f.path("docs/f.txt")).as_deref(), Some("F"));
    let seen: Vec<Marked> = marks.try_iter().collect();
    assert!(
        seen.iter().any(|m| m.ino == unmarked && m.entries == 0 && m.name != "docs"),
        "docs must be marked while empty, before its real name shows it: {seen:?}"
    );
}

/// The same for the holding directory: one left by a cycle whose marking
/// failed is marked before anything is moved into it.
#[test]
fn a_holding_directory_whose_marking_failed_is_marked_before_it_is_used() {
    let f = fixture();
    f.listed(&tree(), true);
    let sockets = tempfile::tempdir().unwrap();
    let refusing = sockets.path().join("refusing.sock");
    let _refused = helper_answering(refusing.clone(), libc::EIO);
    let link = f.handle().block_on(HelperLink::connect(&refusing)).unwrap().0;
    f.store.call_blocking(move |s| { s.begin_staging(true)?; s.stage(&[file("F", "D", "renamed.txt")]) }).unwrap();
    let err = f.materializer(true, Some(link)).apply(Scope::Changed(vec!["F".into()])).unwrap_err();
    assert!(matches!(err, ApplyError::Mark(..)), "{err:?}");
    let holding = ino(&f.path(".konedrive-holding"));

    let socket = sockets.path().join("helper.sock");
    let marks = marking_helper(socket.clone());
    let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
    f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
    assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
    let seen: Vec<Marked> = marks.try_iter().collect();
    assert!(seen.iter().any(|m| m.ino == holding && m.entries == 0), "the holding directory was never marked: {seen:?}");
}

/// One `MarkDir` as the helper saw it.
#[derive(Debug)]
struct Marked {
    ino: u64,
    /// How many entries the directory held at that moment.
    entries: usize,
    /// Its name at that moment.
    name: String,
}

/// Acknowledges everything and reports every `MarkDir` it is sent.
fn marking_helper(path: PathBuf) -> std::sync::mpsc::Receiver<Marked> {
    helper_answering(path, 0)
}

/// Answers every `MarkDir` with `errno` (everything else with success) and
/// reports each one it is sent.
fn helper_answering(path: PathBuf, errno: i32) -> std::sync::mpsc::Receiver<Marked> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
    sock_listen(&fd, Backlog::new(4).unwrap()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let accepted = accept(fd.as_raw_fd()).unwrap();
        // SAFETY: a descriptor `accept` just returned, owned by nothing else.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _ = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, fd)) = channel.recv::<ToHelper>() {
            let mut answer = 0;
            if let (ToHelper::MarkDir, Some(fd)) = (&message, fd) {
                let at = format!("/proc/self/fd/{}", fd.as_raw_fd());
                let entries = std::fs::read_dir(&at).unwrap().count();
                let name = std::fs::read_link(&at).unwrap().file_name().unwrap().to_string_lossy().into_owned();
                let ino = File::from(fd).metadata().unwrap().ino();
                let _ = tx.send(Marked { ino, entries, name });
                answer = errno;
            }
            if channel.send(&ToDaemon::Ack { errno: answer }, None).is_err() {
                break;
            }
        }
    });
    rx
}
