//! A folder served as a FUSE mount, read-only.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the folder on
//! disk holds only the files that are downloaded, and is reached through the descriptor opened
//! before the mount, never through the mount. A file with no object there is not downloaded:
//! opening it downloads it under a staging name and renames it into place. Nothing that changes
//! the folder is accepted from a program.
//!
//! - The requests of the mount read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount ([`Mount::committed`]): the
//!   announcer's thread creates, touches or removes the name by path, the mount answers it from
//!   the store, and the kernel tells whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has no object to carry one.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use konedrive_fs::placeholder::{self, State, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE};
use konedrive_graph::pool::{Class, Size, TransferPool};
use konedrive_tree::{Kind, Placement, Row, Store, Table, TreeStore};
use tokio::sync::oneshot;

use crate::folder::root::SyncRoot;
use crate::hydration::source::{download_staged, ContentSource, Share, Split};
use crate::hydration::tracked::Tracked;
use crate::status::activity::{self, Kind as EventKind};
use crate::status::report::Report;

/// Where downloads are built, inside the folder. No row has this name, so the mount never
/// shows it.
pub const STAGING: &str = ".konedrive-staging";

const TTL: Duration = Duration::from_secs(1);

/// The node ids the kernel knows, each an item of the store. The root is 1.
#[derive(Default)]
struct Nodes {
    items: HashMap<u64, Node>,
    inos: HashMap<String, u64>,
    next: u64,
}

struct Node {
    id: String,
    /// The node above and this one's name there.
    place: (u64, String),
}

impl Nodes {
    fn ino(&mut self, row: &Row, parent: u64) -> u64 {
        let place = (parent, row.name.clone());
        if let Some(i) = self.inos.get(&row.id) {
            self.items.get_mut(i).expect("a node of every known id").place = place;
            return *i;
        }
        self.next += 1;
        self.inos.insert(row.id.clone(), self.next);
        self.items.insert(self.next, Node { id: row.id.clone(), place });
        self.next
    }

    /// The node's path below the folder, from the names the kernel was told.
    fn rel(&self, mut ino: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while ino != 1 {
            let (parent, name) = &self.items.get(&ino)?.place;
            names.push(name.as_str());
            ino = *parent;
        }
        Some(names.iter().rev().collect())
    }
}

/// One item a cycle changed: where the store had it and where the new tree has it.
pub struct Step {
    pub id: String,
    pub old: Option<(Row, PathBuf)>,
    pub new: Option<(Row, PathBuf)>,
}

impl Step {
    /// The item is a file whose content is another now.
    pub fn content_changed(&self) -> bool {
        matches!((&self.old, &self.new), (Some((o, _)), Some((n, _)))
            if n.kind == Kind::File && (o.ctag != n.ctag || o.quickxor != n.quickxor || o.size != n.size))
    }
}

enum Announce {
    /// A name that came: its place, its kind, and the item it lies in.
    Came(PathBuf, Kind, Option<String>),
    /// A name that went: its place, its kind, its item, and the item it lay in.
    Gone(PathBuf, Kind, String, Option<String>),
    Changed(PathBuf, String),
}

/// What the announcer is telling the kernel right now. The kernel looks a name up before it
/// passes on a create or a remove of it, and the store has the change already: so for this one
/// name the announcer is told what was true before, a name that came as absent and one that went
/// as still there.
enum Told {
    Came { parent: u64, name: String },
    Gone { parent: u64, name: String, ino: u64, kind: Kind },
}

/// Who waits for a download: an open of the mount, by its request, or a caller in the daemon.
enum Waiter {
    Open(u64, ReplyOpen),
    Caller(oneshot::Sender<Result<(), i32>>),
}

impl Waiter {
    fn fail(self, errno: i32) {
        match self {
            Waiter::Open(_, reply) => reply.error(Errno::from_i32(errno)),
            Waiter::Caller(tell) => {
                let _ = tell.send(Err(errno));
            }
        }
    }
}

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Freed {
    pub files: u64,
    pub bytes: u64,
    /// Downloaded files left as they are: pinned, or being downloaded.
    pub kept: u64,
}

/// Why the mount refuses what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("not an item of this folder")]
    NotOurs,
    #[error("{path} is pinned by {by}: unpin it first")]
    Pinned { path: String, by: String },
    #[error("{0}")]
    Io(String),
}

struct Shared {
    root: SyncRoot,
    /// The folder under the mount, opened before the mount.
    folder: File,
    /// The store, for reading alone: what the mount's requests ask.
    reader: Mutex<TreeStore>,
    /// The store's one writer, for the pins.
    store: Store,
    nodes: Mutex<Nodes>,
    source: Arc<dyn ContentSource>,
    report: Report,
    pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parts share.
    parts: Arc<Share>,
    /// The downloads in progress, by item, and who waits for each.
    fills: Mutex<HashMap<String, Vec<Waiter>>>,
    /// The items that carry a pin of their own, as the store has them.
    pins: Mutex<HashSet<String>>,
    /// The announcer: its thread, what it is telling, and the way to it.
    announcer: AtomicI32,
    told: Mutex<Option<Told>>,
    tell: Mutex<Option<mpsc::Sender<Vec<Announce>>>>,
    /// What the cycle now committing changed: announced, and the pins of what went taken off,
    /// once it is committed.
    pending: Mutex<(Vec<Announce>, Vec<String>)>,
    /// The mount is being taken off: the announcer says nothing more through it.
    stopping: std::sync::atomic::AtomicBool,
    runtime: tokio::runtime::Handle,
}

fn errno(e: &io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

fn seconds(t: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(t.max(0) as u64)
}

fn other(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

/// Removes the downloaded copy at `path` if it is the copy of `id` and no other file. Its size
/// on disk, when it went.
fn remove_copy(path: &Path, id: &str) -> Option<u64> {
    let file = File::open(path).ok()?;
    if placeholder::read_item_id(&file).ok().flatten().as_deref() != Some(id) {
        return None;
    }
    let bytes = file.metadata().map_or(0, |m| m.blocks() * 512);
    fs::remove_file(path).ok().map(|()| bytes)
}

impl Shared {
    /// A path in the folder under the mount: through the descriptor, never through the mount.
    fn under(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.folder).join(rel)
    }

    fn read<T>(&self, ask: impl FnOnce(&TreeStore) -> Result<T, konedrive_tree::TreeError>) -> Option<T> {
        ask(&crate::panic::lock(&self.reader)).ok()
    }

    fn id(&self, ino: INodeNo) -> Option<String> {
        if ino.0 == 1 {
            return self.read(|s| s.root_item_id()).flatten();
        }
        crate::panic::lock(&self.nodes).items.get(&ino.0).map(|n| n.id.clone())
    }

    fn row(&self, id: &str) -> Option<Row> {
        self.read(|s| s.get(Table::Items, id)).flatten()
    }

    fn rel_of_ino(&self, ino: u64) -> Option<PathBuf> {
        crate::panic::lock(&self.nodes).rel(ino)
    }

    /// The downloaded copy of item `id` at `rel`, if the folder under the mount holds it: a
    /// regular file that carries the item's id. Any other file there is not the item's — one
    /// the user put in the folder while nothing was mounted, or the copy of another item —
    /// and is never shown as its content.
    fn copy(&self, rel: &Path, id: &str) -> Option<fs::Metadata> {
        let path = self.under(rel);
        let meta = fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file)?;
        let carried = xattr::get(&path, XATTR_ITEM_ID).ok().flatten()?;
        (carried == id.as_bytes()).then_some(meta)
    }

    fn has_copy(&self, rel: &Path, id: &str) -> bool {
        self.copy(rel, id).is_some()
    }

    /// What the mount shows of a row: the downloaded file's own size and blocks when the folder
    /// has it, the store's when it does not.
    fn attr(&self, row: &Row, ino: u64) -> FileAttr {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut attr = FileAttr {
            ino: INodeNo(ino),
            size: row.size,
            blocks: 0,
            atime: seconds(row.mtime),
            mtime: seconds(row.mtime),
            ctime: seconds(row.mtime),
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        if row.kind == Kind::Folder {
            attr.kind = FileType::Directory;
            attr.perm = 0o755;
            attr.nlink = 2;
            attr.size = 0;
            return attr;
        }
        if let Some(meta) = self.rel_of_ino(ino).and_then(|rel| self.copy(&rel, &row.id)) {
            attr.size = meta.size();
            attr.blocks = meta.blocks();
        }
        attr
    }

    fn children(&self, id: &str) -> Vec<Row> {
        let rows = self.read(|s| s.children(Table::Items, id)).unwrap_or_default();
        rows.into_iter().filter(|r| r.placement == Placement::Placed).collect()
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Option<Row> {
        let (dir, name) = self.id(parent).zip(name.to_str())?;
        self.read(|s| s.child(Table::Items, &dir, name)).flatten().filter(|r| r.placement == Placement::Placed)
    }

    fn open_downloaded(&self, rel: &Path) -> io::Result<i32> {
        Ok(File::open(self.under(rel))?.into_raw_fd())
    }

    fn announce(&self, what: Vec<Announce>) {
        if what.is_empty() {
            return;
        }
        if let Some(tell) = &*crate::panic::lock(&self.tell) {
            let _ = tell.send(what);
        }
    }

    /// The path a download is shown under: the file's place in the folder.
    fn shown(&self, rel: &Path) -> String {
        self.root.path.join(rel).display().to_string()
    }

    /// Downloads `row` under a staging name, as every download of the daemon is made (breaks,
    /// the version it belongs to, the hash), and renames it into place. Its size, or the errno
    /// it failed with.
    async fn fill(&self, row: &Row, rel: &Path, class: Class) -> Result<u64, i32> {
        let io_errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
        let mut slot = self.pool.acquire_sized(class, Size::of(row.size)).await;
        let staging = self.under(Path::new(STAGING));
        fs::create_dir_all(&staging).map_err(io_errno)?;
        let name = staging.join(format!("{:016x}", {
            let mut hash = 0xcbf29ce484222325u64;
            row.id.bytes().for_each(|b| hash = (hash ^ b as u64).wrapping_mul(0x100000001b3));
            hash
        }));
        // Not emptied: what an earlier download of the item left there is continued from its
        // checkpoint when it is of the same version.
        let file = File::options().read(true).write(true).create(true).truncate(false).open(&name).map_err(io_errno)?;
        let shown = self.shown(rel);
        let tracked = match class {
            Class::Open => Tracked::opening(Arc::clone(&self.source), self.report.transfers.clone(), shown),
            _ => Tracked::new(Arc::clone(&self.source), self.report.transfers.clone(), shown),
        };
        let done = async {
            // A large file that nothing waits to open (a pinned one) goes in parallel parts, the
            // slot held here being its first stream's.
            let split = (class != Class::Open && Size::of(row.size) == Size::Large).then(|| Split::new(Arc::clone(&self.pool), Arc::clone(&self.parts)));
            download_staged(&file, &row.id, row.size, &tracked, split.as_ref()).await?;
            let size = tracked.fetched().unwrap_or(row.size);
            let (id, mtime, target, staged) = (row.id.clone(), row.mtime, self.under(rel), name.clone());
            // The file's own steps wait for the disk: not on the runtime's threads.
            tokio::task::spawn_blocking(move || {
                placeholder::write_item_id(&file, &id)?;
                placeholder::write_state(&file, State::Hydrated)?;
                file.set_modified(seconds(mtime))?;
                file.sync_all()?;
                if let Some(dir) = target.parent() {
                    fs::create_dir_all(dir)?;
                }
                // What stands at the item's place and carries no item's id is the user's own
                // file, put there while nothing was mounted: it is never replaced. A copy of
                // another item left there can be downloaded again, and goes.
                if fs::symlink_metadata(&target).is_ok() && xattr::get(&target, XATTR_ITEM_ID).ok().flatten().is_none() {
                    return Err(io::Error::from_raw_os_error(libc::EEXIST));
                }
                fs::rename(&staged, target)
            })
            .await
            .map_err(|_| libc::EIO)?
            .map_err(io_errno)?;
            Ok(size)
        }
        .await;
        drop(tracked);
        // A download that failed keeps what it has: the next one continues from its checkpoint.
        if done.is_ok() {
            slot.succeeded();
        }
        done
    }

    /// Downloads `row` to `rel` unless a download of it runs already; `waiter` is answered when
    /// the download ends, however it ends.
    fn ensure(self: &Arc<Self>, row: Row, rel: PathBuf, class: Class, waiter: Option<Waiter>) {
        {
            let mut fills = crate::panic::lock(&self.fills);
            if let Some(waiting) = fills.get_mut(&row.id) {
                return waiting.extend(waiter);
            }
            fills.insert(row.id.clone(), waiter.into_iter().collect());
        }
        // `hydrating`, for whoever shows the state.
        self.announce(vec![Announce::Changed(rel.clone(), row.id.clone())]);
        let s = Arc::clone(self);
        self.runtime.spawn(async move {
            // A download that panics still leaves no open waiting for it for ever.
            struct Ended(Arc<Shared>, String);
            impl Drop for Ended {
                fn drop(&mut self) {
                    for waiter in crate::panic::lock(&self.0.fills).remove(&self.1).unwrap_or_default() {
                        waiter.fail(libc::EIO);
                    }
                }
            }
            let _ended = Ended(Arc::clone(&s), row.id.clone());
            let filled = s.fill(&row, &rel, class).await;
            let shown = s.shown(&rel);
            let event = match &filled {
                Ok(size) => activity::event(EventKind::Downloaded, shown, activity::human_size(*size)),
                Err(errno) => activity::event(EventKind::Failed, shown, activity::failure_reason(*errno)),
            };
            s.report.activity.record(vec![event]).await;
            s.report.space.kick();
            let waiting = crate::panic::lock(&s.fills).remove(&row.id).unwrap_or_default();
            for waiter in waiting {
                match waiter {
                    Waiter::Caller(tell) => {
                        let _ = tell.send(filled.map(|_| ()));
                    }
                    Waiter::Open(_, reply) => {
                        let opened = filled.map_err(Errno::from_i32).and_then(|_| s.open_downloaded(&rel).map_err(|e| errno(&e)));
                        match opened {
                            Ok(fd) => reply.opened(FileHandle(fd as u64), FopenFlags::FOPEN_KEEP_CACHE),
                            Err(e) => reply.error(e),
                        }
                    }
                }
            }
            s.announce(vec![Announce::Changed(rel, row.id)]);
        });
    }

    /// The row at `rel`, name by name from the root.
    fn resolve(&self, rel: &Path) -> Option<Row> {
        let store = crate::panic::lock(&self.reader);
        let mut row = store.get(Table::Items, &store.root_item_id().ok()??).ok()??;
        for name in rel.components() {
            row = store.child(Table::Items, &row.id, name.as_os_str().to_str()?).ok()??;
            if row.placement != Placement::Placed {
                return None;
            }
        }
        Some(row)
    }

    /// The item that keeps `row` on this device: itself or a folder above it, whichever
    /// carries a pin.
    fn pinned_by(&self, row: &Row) -> Option<Row> {
        let (pins, store) = (crate::panic::lock(&self.pins), crate::panic::lock(&self.reader));
        let mut at = Some(row.clone());
        while let Some(row) = at {
            if pins.contains(&row.id) {
                return Some(row);
            }
            at = row.parent_id.as_deref().and_then(|p| store.get(Table::Items, p).ok().flatten());
        }
        None
    }

    /// The files at and below `row`, each with its place in the folder.
    fn files_below(&self, row: &Row) -> Vec<(Row, PathBuf)> {
        let store = crate::panic::lock(&self.reader);
        let mut ids = store.descendants(Table::Items, &row.id).unwrap_or_default();
        ids.push(row.id.clone());
        let file = |id: &String| {
            let row = store.get(Table::Items, id).ok().flatten().filter(|r| r.kind == Kind::File)?;
            let at = store.locate(Table::Items, id).ok().flatten().filter(|at| at.placed)?;
            Some((row, at.rel))
        };
        ids.iter().filter_map(file).collect()
    }

    /// Downloads every pinned file that is not downloaded: run when a pin is put on and after
    /// every cycle, so that a new file in a pinned folder and a pinned file whose content
    /// changed are downloaded by it.
    fn sweep(self: &Arc<Self>) {
        let pins: Vec<String> = crate::panic::lock(&self.pins).iter().cloned().collect();
        for id in pins {
            let Some(row) = self.row(&id) else { continue };
            for (file, rel) in self.files_below(&row) {
                if !self.has_copy(&rel, &file.id) {
                    self.ensure(file, rel, Class::Download, None);
                }
            }
        }
    }

    fn state_of(&self, row: &Row, rel: &Path) -> State {
        if crate::panic::lock(&self.fills).contains_key(&row.id) {
            State::Hydrating
        } else if self.has_copy(rel, &row.id) {
            State::Hydrated
        } else {
            State::OnlineOnly
        }
    }
}

/// A folder served as a mount, for as long as this is held.
pub struct Mount {
    shared: Arc<Shared>,
    session: Mutex<Option<fuser::BackgroundSession>>,
    announcer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The mount's connection, as the kernel numbers it (`/sys/fs/fuse/connections/<n>`).
    connection: u32,
}

/// What a mount is made with.
pub struct Parts {
    pub root: SyncRoot,
    /// The folder's tree store: its file, for the mount's own reading connection, and its one
    /// writer.
    pub store_path: PathBuf,
    pub store: Store,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    pub parts: Arc<Share>,
}

/// Takes off a mount a daemon that died left on `folder`: every path in it then fails with
/// "not connected", and nothing can be mounted over it.
pub fn clear_stale(folder: &Path) {
    if let Err(e) = fs::metadata(folder) {
        if e.raw_os_error() == Some(libc::ENOTCONN) {
            tracing::warn!("{} carries the mount of a daemon that is gone; taking it off", folder.display());
            let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(folder).status();
        }
    }
}

impl Mount {
    /// Mounts over `parts.root` and serves it. The folder on disk must carry its root id
    /// already: after this its path is the mount.
    pub async fn start(parts: Parts) -> io::Result<Arc<Mount>> {
        let Parts { root, store_path, store, source, report, pool, parts } = parts;
        let pins: HashSet<String> = store.call(|s| s.pins()).await.map_err(other)?.into_iter().collect();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            clear_stale(&root.path);
            // Before the mount: afterwards this path is the mount.
            let folder = File::open(&root.path)?;
            // What downloads that never ended left is kept for a week, to be continued from.
            if let Ok(staged) = fs::read_dir(konedrive_fs::proc_path(&folder).join(STAGING)) {
                let old = |e: &fs::DirEntry| e.metadata().and_then(|m| m.modified()).is_ok_and(|at| at.elapsed().is_ok_and(|age| age > Duration::from_secs(7 * 24 * 3600)));
                staged.flatten().filter(old).for_each(|e| {
                    let _ = fs::remove_file(e.path());
                });
            }
            let reader = TreeStore::open_read_only(&store_path).map_err(other)?;
            let shared = Arc::new(Shared {
                root: root.clone(),
                folder,
                reader: Mutex::new(reader),
                store,
                nodes: Mutex::new(Nodes { next: 1, ..Nodes::default() }),
                source,
                report,
                pool,
                parts,
                fills: Mutex::default(),
                pins: Mutex::new(pins),
                announcer: AtomicI32::new(0),
                told: Mutex::new(None),
                tell: Mutex::new(None),
                pending: Mutex::default(),
                stopping: std::sync::atomic::AtomicBool::new(false),
                runtime,
            });
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::FSName("konedrive".into()),
                MountOption::Subtype("konedrive".into()),
                MountOption::NoSuid,
                MountOption::NoDev,
            ];
            config.n_threads = Some(4);
            config.clone_fd = true;
            let session = fuser::Session::new(Served(Arc::clone(&shared)), &root.path, &config)?;
            let notifier = session.notifier();
            let (tell, told) = mpsc::channel::<Vec<Announce>>();
            *crate::panic::lock(&shared.tell) = Some(tell);
            let session = match session.spawn() {
                Ok(session) => session,
                Err(e) => {
                    *crate::panic::lock(&shared.tell) = None;
                    return Err(e);
                }
            };
            let connection = fs::metadata(&root.path).map_or(0, |m| libc::minor(m.dev()));
            let saying = Arc::clone(&shared);
            let announcer = std::thread::Builder::new().name("konedrive-announce".into()).spawn(move || announce(&saying, &notifier, told))?;
            shared.sweep();
            Ok(Arc::new(Mount { shared, session: Mutex::new(Some(session)), announcer: Mutex::new(Some(announcer)), connection }))
        })
        .await
        .map_err(other)?
    }

    /// Takes the mount off. The folder on disk then shows the downloaded files alone.
    ///
    /// In this order: the announcer stops while the mount still answers it (what it says by
    /// path would otherwise land in the folder itself), every open that waits for a download
    /// is failed, and the connection is aborted before the mount is detached — a program that
    /// still holds a file of the mount open would otherwise keep the session, and this call,
    /// waiting for it.
    pub fn stop(&self) {
        let Some(session) = crate::panic::lock(&self.session).take() else { return };
        let s = &self.shared;
        s.stopping.store(true, Ordering::SeqCst);
        *crate::panic::lock(&s.tell) = None;
        if let Some(announcer) = crate::panic::lock(&self.announcer).take() {
            let _ = announcer.join();
        }
        let waiting: Vec<Waiter> = crate::panic::lock(&s.fills).drain().flat_map(|(_, waiting)| waiting).collect();
        waiting.into_iter().for_each(|waiter| waiter.fail(libc::EIO));
        if let Err(e) = fs::write(format!("/sys/fs/fuse/connections/{}/abort", self.connection), "1") {
            tracing::warn!("the connection of {} could not be aborted ({e}); its mount is taken off all the same", s.root.path.display());
            // Not waited for: it ends when the last program lets go of the mount.
            std::thread::spawn(move || {
                let _ = session.umount_and_join();
            });
            return;
        }
        if let Err(e) = session.umount_and_join() {
            tracing::warn!("taking the mount off {}: {e}", s.root.path.display());
        }
    }

    /// [`stop`](Self::stop), for a folder that is forgotten: what downloads that never ended
    /// left goes too, and the folder is the user's again.
    pub fn forget(&self) {
        self.stop();
        let _ = fs::remove_dir_all(self.shared.under(Path::new(STAGING)));
    }

    /// The downloads that never ended, as the folder under the mount holds them: each one's
    /// size on disk and its checkpoint.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn staged(&self) -> Vec<(u64, Option<u64>)> {
        let Ok(staged) = fs::read_dir(self.shared.under(Path::new(STAGING))) else { return Vec::new() };
        let seen = |e: fs::DirEntry| {
            let file = File::open(e.path()).ok()?;
            Some((file.metadata().ok()?.len(), placeholder::read_progress(&file).ok().flatten().map(|p| p.bytes)))
        };
        staged.flatten().filter_map(seen).collect()
    }

    /// The folder under the mount, for what must not go through the mount: a walk of what is
    /// downloaded.
    pub fn under(&self) -> PathBuf {
        konedrive_fs::proc_path(&self.shared.folder)
    }

    /// Makes the folder under the mount follow the cycle that `store` has staged and not yet
    /// committed: a downloaded copy whose content is stale or whose item is gone is removed,
    /// and one whose item moved moves with it. A folder that is gone is left where it is; the
    /// mount no longer shows it. Returns what changed; it is announced by
    /// [`committed`](Self::committed).
    pub fn follow(&self, store: &TreeStore) -> Result<Vec<Step>, konedrive_tree::TreeError> {
        let placed = |table: Table, id: &str| {
            let row = store.get(table, id).ok().flatten()?;
            let at = store.locate(table, id).ok().flatten()?;
            at.placed.then_some((row, at.rel))
        };
        let steps: Vec<Step> =
            store.changed_ids()?.into_iter().map(|id| Step { old: placed(Table::Items, &id), new: placed(Table::Staging, &id), id }).collect();
        let s = &self.shared;
        // The copy of item `id` at `rel`, or nothing: a file that is not that item's download
        // is never moved or removed.
        let ours = |rel: &Path, id: &str, kind: Kind| match kind {
            Kind::File => s.has_copy(rel, id),
            Kind::Folder => fs::symlink_metadata(s.under(rel)).is_ok_and(|m| m.is_dir()),
        };
        let (mut told, mut unpinned) = (Vec::new(), Vec::new());
        for step in &steps {
            match (&step.old, &step.new) {
                (Some((old, was)), None) => {
                    if old.kind == Kind::File {
                        remove_copy(&s.under(was), &step.id);
                    }
                    unpinned.push(step.id.clone());
                    told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                }
                (None, Some((new, now))) => told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone())),
                (Some((old, was)), Some((new, now))) => {
                    if step.content_changed() {
                        remove_copy(&s.under(was), &step.id);
                    } else if was != now && ours(was, &step.id, old.kind) && fs::symlink_metadata(s.under(now)).is_err() {
                        let _ = s.under(now).parent().map(fs::create_dir_all);
                        let _ = fs::rename(s.under(was), s.under(now));
                    }
                    if was != now {
                        told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                        told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone()));
                    } else if new.kind == Kind::File {
                        told.push(Announce::Changed(now.clone(), step.id.clone()));
                    }
                }
                (None, None) => {}
            }
        }
        // Only this cycle's: what an earlier one left was never committed.
        *crate::panic::lock(&s.pending) = (told, unpinned);
        Ok(steps)
    }

    /// The cycle [`follow`](Self::follow) followed is committed: its changes are announced
    /// through the mount, and what is pinned and not downloaded is downloaded.
    pub fn committed(&self) {
        let (told, unpinned) = std::mem::take(&mut *crate::panic::lock(&self.shared.pending));
        // An item that is gone keeps nothing on this device.
        let dropped: Vec<String> = {
            let mut pins = crate::panic::lock(&self.shared.pins);
            unpinned.into_iter().filter(|id| pins.remove(id)).collect()
        };
        if !dropped.is_empty() {
            let store = self.shared.store.clone();
            self.shared.runtime.spawn(async move {
                let _ = store.call(move |s| dropped.iter().try_for_each(|id| s.set_pin(id, false))).await;
            });
        }
        self.shared.announce(told);
        self.shared.sweep();
    }

    fn rel(&self, path: &Path) -> Result<PathBuf, Refused> {
        path.strip_prefix(&self.shared.root.path).map(Path::to_owned).map_err(|_| Refused::NotOurs)
    }

    fn item(&self, path: &Path) -> Result<(Row, PathBuf), Refused> {
        let rel = self.rel(path)?;
        let row = self.shared.resolve(&rel).ok_or(Refused::NotOurs)?;
        Ok((row, rel))
    }

    /// Downloads the file at `path` now, and waits for it.
    pub async fn hydrate(&self, path: &Path) -> Result<(), Refused> {
        let (row, rel) = self.item(path)?;
        if row.kind != Kind::File {
            return Err(Refused::NotOurs);
        }
        if self.shared.has_copy(&rel, &row.id) {
            return Ok(());
        }
        let (tell, told) = oneshot::channel();
        self.shared.ensure(row, rel, Class::Open, Some(Waiter::Caller(tell)));
        match told.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(Refused::Io(format!("hydration failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(Refused::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded files at and below `path`. What a pin keeps, or a
    /// download is filling, is left and counted. Whether `path` itself may be freed is the
    /// caller's to ask first ([`look`](Self::look)).
    pub async fn free_up(&self, path: &Path) -> Result<Freed, Refused> {
        let (row, _) = self.item(path)?;
        Ok(self.free_below(row).await)
    }

    /// Frees the space of every downloaded file that no pin keeps.
    pub async fn free_up_all(&self) -> Freed {
        let Some(root) = self.shared.id(INodeNo(1)).and_then(|id| self.shared.row(&id)) else { return Freed::default() };
        self.free_below(root).await
    }

    async fn free_below(&self, row: Row) -> Freed {
        let s = Arc::clone(&self.shared);
        let freed = tokio::task::spawn_blocking(move || {
            let (mut freed, mut told) = (Freed::default(), Vec::new());
            for (file, at) in s.files_below(&row) {
                if !s.has_copy(&at, &file.id) {
                    continue;
                }
                if s.pinned_by(&file).is_some() || crate::panic::lock(&s.fills).contains_key(&file.id) {
                    freed.kept += 1;
                    continue;
                }
                match remove_copy(&s.under(&at), &file.id) {
                    Some(bytes) => {
                        freed.files += 1;
                        freed.bytes += bytes;
                        told.push(Announce::Changed(at, file.id));
                    }
                    None => freed.kept += 1,
                }
            }
            s.announce(told);
            freed
        })
        .await
        .unwrap_or_default();
        self.shared.report.space.kick();
        freed
    }

    /// Puts the pin on the item at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), Refused> {
        let (row, rel) = self.item(path)?;
        let id = row.id.clone();
        self.shared.store.call(move |s| s.set_pin(&id, pinned)).await.map_err(|e| Refused::Io(e.to_string()))?;
        let changed = {
            let mut pins = crate::panic::lock(&self.shared.pins);
            if pinned { pins.insert(row.id.clone()) } else { pins.remove(&row.id) }
        };
        if changed {
            self.shared.announce(vec![Announce::Changed(rel, row.id)]);
        }
        if pinned {
            self.shared.sweep();
        }
        Ok(())
    }

    /// The state of the item at `path`, its own pin, and the folder above that pins it.
    pub fn look(&self, path: &Path) -> Result<Looked, Refused> {
        let (row, rel) = self.item(path)?;
        let s = &self.shared;
        let own_pin = crate::panic::lock(&s.pins).contains(&row.id);
        let above = row.parent_id.as_deref().and_then(|p| s.row(p)).and_then(|above| s.pinned_by(&above));
        let pinned_above = above.and_then(|by| s.read(|store| store.locate(Table::Items, &by.id)).flatten()).map(|at| s.root.path.join(at.rel));
        let state = (row.kind == Kind::File).then(|| s.state_of(&row, &rel));
        Ok(Looked { id: row.id, folder: row.kind == Kind::Folder, state, own_pin, pinned_above })
    }

    /// How many items carry a pin of their own.
    pub fn pinned_count(&self) -> u64 {
        crate::panic::lock(&self.shared.pins).len() as u64
    }

    /// How many downloaded files, and bytes, the items at and below `path` hold.
    pub fn downloaded_under(&self, path: &Path) -> Result<(u64, u64), Refused> {
        let (row, _) = self.item(path)?;
        let s = &self.shared;
        let sizes = s.files_below(&row).into_iter().filter_map(|(file, at)| s.copy(&at, &file.id));
        Ok(sizes.fold((0, 0), |(files, bytes), m| (files + 1, bytes + m.blocks() * 512)))
    }
}

/// What [`Mount::look`] saw of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Looked {
    pub id: String,
    pub folder: bool,
    /// `None` for a folder.
    pub state: Option<State>,
    pub own_pin: bool,
    /// The folder above that pins it.
    pub pinned_above: Option<PathBuf>,
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The announcer's thread: each change is made by path through the mount, which answers it from
/// the store ([`Told`]), so that the kernel tells whoever watches the directory.
fn announce(shared: &Arc<Shared>, notifier: &fuser::Notifier, told: mpsc::Receiver<Vec<Announce>>) {
    shared.announcer.store(unsafe { libc::gettid() }, Ordering::SeqCst);
    let path = |rel: &Path| CString::new(shared.root.path.join(rel).into_os_string().into_encoded_bytes()).unwrap_or_default();
    let name = |rel: &Path| rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tell = |told: Told| *crate::panic::lock(&shared.told) = Some(told);
    // The node of an item the kernel was told: the root is always known.
    let node = |id: &Option<String>| -> Option<u64> {
        let id = id.as_deref()?;
        if shared.read(|s| s.root_item_id()).flatten().as_deref() == Some(id) {
            return Some(1);
        }
        crate::panic::lock(&shared.nodes).inos.get(id).copied()
    };
    for a in told.into_iter().flatten() {
        // The mount is going: nothing more is said through it.
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        // SAFETY: plain system calls on a path this function owns.
        unsafe {
            match a {
                Announce::Came(rel, kind, above) => {
                    // A directory the kernel was never told has nobody watching it.
                    let Some(parent) = node(&above) else { continue };
                    tell(Told::Came { parent, name: name(&rel) });
                    match kind {
                        Kind::Folder => libc::mkdir(path(&rel).as_ptr(), 0o755),
                        Kind::File => libc::mknod(path(&rel).as_ptr(), libc::S_IFREG | 0o644, 0),
                    };
                }
                Announce::Gone(rel, kind, id, above) => {
                    // A name the kernel was never told has nobody watching it go.
                    let (Some(parent), Some(ino)) = (node(&above), crate::panic::lock(&shared.nodes).inos.get(&id).copied()) else { continue };
                    tell(Told::Gone { parent, name: name(&rel), ino, kind });
                    match kind {
                        Kind::Folder => libc::rmdir(path(&rel).as_ptr()),
                        Kind::File => libc::unlink(path(&rel).as_ptr()),
                    };
                }
                Announce::Changed(rel, id) => {
                    // The kernel's cached pages of the old content go first.
                    let ino = crate::panic::lock(&shared.nodes).inos.get(&id).copied();
                    if let Some(ino) = ino {
                        let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                    }
                    libc::utimensat(libc::AT_FDCWD, path(&rel).as_ptr(), std::ptr::null(), 0);
                }
            }
        }
        *crate::panic::lock(&shared.told) = None;
    }
}

fn borrow(fh: FileHandle) -> std::mem::ManuallyDrop<File> {
    // SAFETY: the handle is a descriptor this mount opened and closes at the release.
    std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fh.0 as i32) })
}

/// The mount's requests.
struct Served(Arc<Shared>);

impl Served {
    fn announcing(&self, req: &Request) -> bool {
        req.pid() as i32 == self.0.announcer.load(Ordering::SeqCst)
    }

    /// The entry `name` of `parent`, as the store has it now.
    fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.child(parent, name) {
            Some(row) => {
                let ino = crate::panic::lock(&self.0.nodes).ino(&row, parent.0);
                reply.entry(&TTL, &self.0.attr(&row, ino), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn xattr(&self, ino: INodeNo, name: &str) -> Option<String> {
        let s = &self.0;
        if ino.0 == 1 && (name == XATTR_ROOT || name == XATTR_DRIVE) {
            // The folder's own: what its registration and its first cycle wrote on it.
            return xattr::FileExt::get_xattr(&s.folder, name).ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned());
        }
        let row = s.id(ino).and_then(|id| s.row(&id))?;
        match name {
            XATTR_ITEM_ID => Some(row.id),
            XATTR_STATE if row.kind == Kind::File => Some(s.state_of(&row, &s.rel_of_ino(ino.0)?).as_str().to_owned()),
            XATTR_PIN if crate::panic::lock(&s.pins).contains(&row.id) => Some("1".to_owned()),
            _ => None,
        }
    }
}

impl Filesystem for Served {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.announcing(req) {
            match &*crate::panic::lock(&self.0.told) {
                Some(Told::Came { parent: in_, name: came }) if *in_ == parent.0 && name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { parent: in_, name: gone, ino, kind }) if *in_ == parent.0 && name == gone.as_str() => {
                    let row = Row { id: String::new(), parent_id: None, name: gone.clone(), kind: *kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
                    return reply.entry(&Duration::ZERO, &self.0.attr(&row, *ino), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let s = &self.0;
        if ino.0 == 1 {
            let root = Row { id: String::new(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
            return reply.attr(&TTL, &s.attr(&root, 1));
        }
        match s.id(ino).and_then(|id| s.row(&id)) {
            Some(row) => reply.attr(&TTL, &s.attr(&row, ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    /// The program behind a waiting open was interrupted or killed: that open is answered now.
    /// The download goes on for whoever else waits, and for the next open.
    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        let mut fills = crate::panic::lock(&self.0.fills);
        for waiting in fills.values_mut() {
            if let Some(at) = waiting.iter().position(|w| matches!(w, Waiter::Open(u, _) if *u == unique.0)) {
                if let Waiter::Open(_, reply) = waiting.swap_remove(at) {
                    reply.error(Errno::EINTR);
                }
                return;
            }
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let s = &self.0;
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return reply.error(Errno::EROFS);
        }
        let (Some(row), Some(rel)) = (s.id(ino).and_then(|id| s.row(&id)), s.rel_of_ino(ino.0)) else {
            return reply.error(Errno::ENOENT);
        };
        if s.has_copy(&rel, &row.id) {
            return match s.open_downloaded(&rel) {
                Ok(fd) => reply.opened(FileHandle(fd as u64), FopenFlags::FOPEN_KEEP_CACHE),
                Err(e) => reply.error(errno(&e)),
            };
        }
        // Not downloaded: one download per item, and every open of it waits for that one.
        s.ensure(row, rel, Class::Open, Some(Waiter::Open(req.unique().0, reply)));
    }

    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        let mut buf = vec![0u8; size as usize];
        let mut got = 0;
        while got < buf.len() {
            match borrow(fh).read_at(&mut buf[got..], offset + got as u64) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => return reply.error(errno(&e)),
            }
        }
        reply.data(&buf[..got]);
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        // SAFETY: the descriptor this mount opened for the handle, closed once.
        unsafe { libc::close(fh.0 as i32) };
        reply.ok();
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let s = &self.0;
        let Some(id) = s.id(ino) else { return reply.ok() };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = crate::panic::lock(&s.nodes).ino(row, ino.0);
            let kind = if row.kind == Kind::Folder { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, &row.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.0;
        let Some(id) = s.id(ino) else { return reply.ok() };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = crate::panic::lock(&s.nodes).ino(row, ino.0);
            if reply.add(INodeNo(child), i as u64 + 1, &row.name, &TTL, &s.attr(row, child), Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // SAFETY: a plain structure the call fills.
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: the folder's own descriptor.
        if unsafe { libc::fstatvfs(self.0.folder.as_raw_fd(), &mut v) } != 0 {
            return reply.error(errno(&io::Error::last_os_error()));
        }
        reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32);
    }

    /// The attributes the Dolphin plugins, `konedrivectl` and the daemon read, answered from
    /// the store.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(value) = name.to_str().and_then(|name| self.xattr(ino, name)) else { return reply.error(Errno::ENODATA) };
        match size {
            0 => reply.size(value.len() as u32),
            n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
            _ => reply.data(value.as_bytes()),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN];
        let held: Vec<u8> = names.iter().filter(|n| self.xattr(ino, n).is_some()).flat_map(|n| n.bytes().chain(std::iter::once(0))).collect();
        match size {
            0 => reply.size(held.len() as u32),
            n if (n as usize) < held.len() => reply.error(Errno::ERANGE),
            _ => reply.data(&held),
        }
    }

    // Nothing that changes the folder is accepted from a program. Refused here and not by a
    // read-only mount: on one the kernel would refuse the announcer too.

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, _size: Option<u64>, _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if self.announcing(req) {
            return self.getattr(req, ino, None, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) && self.0.child(parent, name).is_none() {
            return reply.ok();
        }
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) && self.0.child(parent, name).is_none() {
            return reply.ok();
        }
        reply.error(Errno::EROFS);
    }

    fn rename(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _newparent: INodeNo, _newname: &OsStr, _flags: RenameFlags, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn create(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        reply.error(Errno::EROFS);
    }

    fn write(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _offset: u64, _data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        reply.error(Errno::EROFS);
    }

    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}
