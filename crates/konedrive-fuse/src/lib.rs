//! The folder as a FUSE mount, stage 1: read-only.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the folder on
//! disk holds only the files that are downloaded, and is reached through the descriptor opened
//! before the mount. A file with no object there is not downloaded: opening it downloads it
//! under a staging name and renames it into place. Nothing that changes the folder is accepted.

pub mod cloud;

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_graph::quickxor::{self, QuickXor};
use konedrive_tree::{Kind, Placement, Row, Table, TreeStore};
use konedrived::hydration::source::ContentSource;
use tokio::io::AsyncReadExt;

/// Where downloads are built, inside the folder. No row has this name, so the mount never
/// shows it.
pub const STAGING: &str = ".konedrive-staging";

const TTL: Duration = Duration::from_secs(1);

/// The thread that announces OneDrive's changes through the mount (0: none yet). What it asks
/// of the mount changes nothing: the store has the change already, and the request is there for
/// the kernel to tell whoever watches the directory.
static ANNOUNCER: AtomicI32 = AtomicI32::new(0);

/// What the announcer is telling the kernel right now. The kernel looks a name up before it
/// passes on a create or a remove of it, and the store has the change already: so for this one
/// name the announcer is told what was true before, a name that came as absent and one that went
/// as still there.
enum Told {
    Came(String),
    Gone { name: String, ino: u64, kind: Kind },
}

static TOLD: Mutex<Option<Told>> = Mutex::new(None);

fn announcing(req: &Request) -> bool {
    req.pid() as i32 == ANNOUNCER.load(Ordering::SeqCst)
}

/// The node ids the kernel knows, each an item of the store.
#[derive(Default)]
struct Nodes {
    items: HashMap<u64, Node>,
    inos: HashMap<String, u64>,
    next: u64,
}

struct Node {
    id: String,
    /// The node above and this one's name there; `None` for the root.
    place: Option<(u64, String)>,
}

impl Nodes {
    fn ino(&mut self, row: &Row, parent: u64) -> u64 {
        let place = Some((parent, row.name.clone()));
        if let Some(i) = self.inos.get(&row.id) {
            self.items.get_mut(i).unwrap().place = place;
            return *i;
        }
        self.next += 1;
        self.inos.insert(row.id.clone(), self.next);
        self.items.insert(self.next, Node { id: row.id.clone(), place });
        self.next
    }

    /// The item's path below the folder, from the names the kernel was told.
    fn rel(&self, mut ino: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while let Some((parent, name)) = &self.items.get(&ino)?.place {
            names.push(name.as_str());
            ino = *parent;
        }
        Some(names.iter().rev().collect())
    }
}

struct Shared {
    /// The folder under the mount, opened before the mount.
    folder: File,
    store: Arc<Mutex<TreeStore>>,
    nodes: Mutex<Nodes>,
    source: Arc<dyn ContentSource>,
    /// The downloads in progress, by item, and the opens that wait for each.
    fills: Mutex<HashMap<String, Vec<ReplyOpen>>>,
}

pub struct ReadOnlyFs {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Handle,
}

fn errno(e: &io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

fn seconds(t: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(t.max(0) as u64)
}

impl Shared {
    /// A path in the folder under the mount: through the descriptor, never through the mount.
    fn under(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.folder).join(rel)
    }

    fn id(&self, ino: INodeNo) -> Option<String> {
        self.nodes.lock().unwrap().items.get(&ino.0).map(|n| n.id.clone())
    }

    fn row(&self, id: &str) -> Option<Row> {
        self.store.lock().unwrap().get(Table::Items, id).ok().flatten()
    }

    /// What the mount shows of a row: the downloaded file's own size and times when the folder
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
        let rel = self.nodes.lock().unwrap().rel(ino);
        if let Some(meta) = rel.and_then(|rel| fs::symlink_metadata(self.under(&rel)).ok()) {
            if meta.is_file() {
                attr.size = meta.size();
                attr.blocks = meta.blocks();
                attr.mtime = meta.modified().unwrap_or(attr.mtime);
            }
        }
        attr
    }

    fn downloaded(&self, ino: u64) -> bool {
        let rel = self.nodes.lock().unwrap().rel(ino);
        rel.is_some_and(|rel| fs::symlink_metadata(self.under(&rel)).is_ok_and(|m| m.is_file()))
    }

    fn children(&self, id: &str) -> Vec<Row> {
        let rows = self.store.lock().unwrap().children(Table::Items, id).unwrap_or_default();
        rows.into_iter().filter(|r| r.placement == Placement::Placed).collect()
    }

    fn open_downloaded(&self, rel: &Path) -> io::Result<i32> {
        Ok(File::open(self.under(rel))?.into_raw_fd())
    }

    /// Downloads `row` under a staging name, checks it, and renames it into place.
    async fn fill(&self, row: &Row, rel: &Path) -> io::Result<()> {
        let other = |m: String| io::Error::other(m);
        let fetched = self.source.fetch(&row.id, 0, None).await.map_err(|e| other(e.to_string()))?;
        if fetched.served_from != 0 {
            return Err(other(format!("the source starts at {}, not at 0", fetched.served_from)));
        }
        let staging = self.under(Path::new(STAGING));
        fs::create_dir_all(&staging)?;
        let name = staging.join(format!("{:016x}", {
            let mut hash = 0xcbf29ce484222325u64;
            row.id.bytes().for_each(|b| hash = (hash ^ b as u64).wrapping_mul(0x100000001b3));
            hash
        }));
        let file = File::create(&name)?;
        let done = async {
            let (mut stream, mut hash, mut at, mut buf) = (fetched.stream, QuickXor::new(), 0u64, vec![0u8; 1 << 20]);
            loop {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                file.write_all_at(&buf[..n], at)?;
                hash.update(&buf[..n]);
                at += n as u64;
            }
            if at != fetched.size {
                return Err(other(format!("{at} bytes came of {}", fetched.size)));
            }
            // The hash OneDrive gives with the bytes, or the listed one when it gives none.
            let listed = row.quickxor.as_deref().and_then(quickxor::decode_base64);
            if let Some(want) = fetched.version.as_ref().and_then(|v| v.quick_xor).or(listed) {
                if hash.finish() != want {
                    return Err(other("the content does not match the item's hash".into()));
                }
            }
            konedrive_fs::placeholder::write_item_id(&file, &row.id)?;
            konedrive_fs::placeholder::write_state(&file, State::Hydrated)?;
            file.set_modified(seconds(row.mtime))?;
            file.sync_all()?;
            let target = self.under(rel);
            if let Some(dir) = target.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::rename(&name, target)
        }
        .await;
        if done.is_err() {
            let _ = fs::remove_file(&name);
        }
        done
    }
}

impl ReadOnlyFs {
    pub fn new(folder: File, store: Arc<Mutex<TreeStore>>, source: Arc<dyn ContentSource>, runtime: tokio::runtime::Handle) -> io::Result<Self> {
        let root = store.lock().unwrap().root_item_id().ok().flatten().ok_or_else(|| io::Error::other("the store has no root"))?;
        let mut nodes = Nodes { next: 1, ..Nodes::default() };
        nodes.inos.insert(root.clone(), 1);
        nodes.items.insert(1, Node { id: root, place: None });
        let shared = Shared { folder, store, nodes: Mutex::new(nodes), source, fills: Mutex::default() };
        Ok(Self { shared: Arc::new(shared), runtime })
    }
}

/// What a served folder follows OneDrive with: the drive and how often it is asked.
pub struct Follow {
    pub drive: konedrive_graph::drive::DriveClient,
    pub every: Duration,
}

enum Announce {
    Came(PathBuf, Kind),
    Gone(PathBuf, Kind, String),
    Changed(PathBuf, String),
}

fn announcements(steps: &[cloud::Step]) -> Vec<Announce> {
    let mut out = Vec::new();
    for step in steps {
        match (&step.old, &step.new) {
            (Some((old, was)), None) => out.push(Announce::Gone(was.clone(), old.kind, step.id.clone())),
            (None, Some((new, now))) => out.push(Announce::Came(now.clone(), new.kind)),
            (Some((old, was)), Some((new, now))) if was != now => {
                out.push(Announce::Gone(was.clone(), old.kind, step.id.clone()));
                out.push(Announce::Came(now.clone(), new.kind));
            }
            (Some(_), Some((new, now))) if new.kind == Kind::File => out.push(Announce::Changed(now.clone(), step.id.clone())),
            _ => {}
        }
    }
    out
}

/// Mounts `store` over `folder` and serves it until the mount goes. With `follow`, OneDrive is
/// listed into the store first and asked again every so often; each change is then announced
/// through the mount, so that an inotify watch on a directory sees it.
pub fn serve(folder: &Path, store: TreeStore, source: Arc<dyn ContentSource>, runtime: tokio::runtime::Handle, follow: Option<Follow>) -> io::Result<()> {
    // Before the mount: afterwards this path is the mount.
    let under = File::open(folder)?;
    let under_path = konedrive_fs::proc_path(&under);
    let store = Arc::new(Mutex::new(store));
    if let Some(follow) = &follow {
        let listed = runtime.block_on(cloud::sync_once(&follow.drive, &store, &under_path))?;
        eprintln!("konedrive-fuse: listed, {} items changed", listed.len());
    }
    let fs = ReadOnlyFs::new(under, store.clone(), source, runtime.clone())?;
    let shared = fs.shared.clone();
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("konedrive".into()),
        MountOption::Subtype("konedrive".into()),
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    config.n_threads = Some(4);
    config.clone_fd = true;
    let session = fuser::Session::new(fs, folder, &config)?;
    if let Some(follow) = follow {
        let notifier = session.notifier();
        let (tell, told) = std::sync::mpsc::channel::<Vec<Announce>>();
        let mount = folder.to_owned();
        std::thread::spawn(move || {
            ANNOUNCER.store(unsafe { libc::gettid() }, Ordering::SeqCst);
            let path = |rel: &Path| CString::new(mount.join(rel).into_os_string().into_encoded_bytes()).unwrap();
            for batch in told {
                for a in batch {
                    let name = |rel: &Path| rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let tell = |told: Told| *TOLD.lock().unwrap() = Some(told);
                    unsafe {
                        match a {
                            Announce::Came(rel, kind) => {
                                tell(Told::Came(name(&rel)));
                                match kind {
                                    Kind::Folder => libc::mkdir(path(&rel).as_ptr(), 0o755),
                                    Kind::File => libc::mknod(path(&rel).as_ptr(), libc::S_IFREG | 0o644, 0),
                                };
                            }
                            Announce::Gone(rel, kind, id) => {
                                // A name the kernel was never told has nobody watching it go.
                                let Some(ino) = shared.nodes.lock().unwrap().inos.get(&id).copied() else { continue };
                                tell(Told::Gone { name: name(&rel), ino, kind });
                                match kind {
                                    Kind::Folder => libc::rmdir(path(&rel).as_ptr()),
                                    Kind::File => libc::unlink(path(&rel).as_ptr()),
                                };
                            }
                            Announce::Changed(rel, id) => {
                                // The kernel's cached pages of the old content go first.
                                if let Some(ino) = shared.nodes.lock().unwrap().inos.get(&id) {
                                    let _ = notifier.inval_inode(INodeNo(*ino), 0, 0);
                                }
                                libc::utimensat(libc::AT_FDCWD, path(&rel).as_ptr(), std::ptr::null(), 0);
                            }
                        }
                    }
                    *TOLD.lock().unwrap() = None;
                }
            }
        });
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(follow.every).await;
                match cloud::sync_once(&follow.drive, &store, &under_path).await {
                    Ok(steps) if steps.is_empty() => {}
                    Ok(steps) => {
                        eprintln!("konedrive-fuse: {} items changed in OneDrive", steps.len());
                        let _ = tell.send(announcements(&steps));
                    }
                    Err(e) => eprintln!("konedrive-fuse: asking OneDrive failed: {e}"),
                }
            }
        });
    }
    session.run()
}

fn borrow(fh: FileHandle) -> std::mem::ManuallyDrop<File> {
    std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fh.0 as i32) })
}

impl Filesystem for ReadOnlyFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if announcing(req) {
            match &*TOLD.lock().unwrap() {
                Some(Told::Came(came)) if name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { name: gone, ino, kind }) if name == gone.as_str() => {
                    let row = Row { id: String::new(), parent_id: None, name: gone.clone(), kind: *kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
                    return reply.entry(&Duration::ZERO, &self.shared.attr(&row, *ino), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let s = &self.shared;
        match s.id(ino).and_then(|id| s.row(&id)) {
            Some(row) => reply.attr(&TTL, &s.attr(&row, ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let s = self.shared.clone();
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return reply.error(Errno::EROFS);
        }
        let (Some(row), Some(rel)) = (s.id(ino).and_then(|id| s.row(&id)), s.nodes.lock().unwrap().rel(ino.0)) else {
            return reply.error(Errno::ENOENT);
        };
        if s.downloaded(ino.0) {
            return match s.open_downloaded(&rel) {
                Ok(fd) => reply.opened(FileHandle(fd as u64), FopenFlags::FOPEN_KEEP_CACHE),
                Err(e) => reply.error(errno(&e)),
            };
        }
        // Not downloaded: one download per item, and every open of it waits for that one.
        {
            let mut fills = s.fills.lock().unwrap();
            if let Some(waiting) = fills.get_mut(&row.id) {
                return waiting.push(reply);
            }
            fills.insert(row.id.clone(), vec![reply]);
        }
        self.runtime.spawn(async move {
            let filled = s.fill(&row, &rel).await;
            let waiting = s.fills.lock().unwrap().remove(&row.id).unwrap_or_default();
            for reply in waiting {
                match filled.as_ref().map_err(errno).and_then(|()| s.open_downloaded(&rel).map_err(|e| errno(&e))) {
                    Ok(fd) => reply.opened(FileHandle(fd as u64), FopenFlags::FOPEN_KEEP_CACHE),
                    Err(e) => reply.error(e),
                }
            }
        });
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
        unsafe { libc::close(fh.0 as i32) };
        reply.ok();
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let s = &self.shared;
        let Some(id) = s.id(ino) else { return reply.error(Errno::ENOENT) };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = s.nodes.lock().unwrap().ino(row, ino.0);
            let kind = if row.kind == Kind::Folder { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, &row.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.shared;
        let Some(id) = s.id(ino) else { return reply.error(Errno::ENOENT) };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = s.nodes.lock().unwrap().ino(row, ino.0);
            if reply.add(INodeNo(child), i as u64 + 1, &row.name, &TTL, &s.attr(row, child), Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatvfs(self.shared.folder.as_raw_fd(), &mut v) } != 0 {
            return reply.error(errno(&io::Error::last_os_error()));
        }
        reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32);
    }

    /// The attributes the Dolphin plugins and `konedrivectl` read, answered from the store.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let s = &self.shared;
        let Some(row) = s.id(ino).and_then(|id| s.row(&id)) else { return reply.error(Errno::ENOENT) };
        let value = match name.to_str() {
            Some(XATTR_ITEM_ID) => row.id.clone(),
            Some(XATTR_STATE) if row.kind == Kind::File => {
                if s.downloaded(ino.0) { State::Hydrated } else { State::OnlineOnly }.as_str().to_owned()
            }
            _ => return reply.error(Errno::ENODATA),
        };
        match size {
            0 => reply.size(value.len() as u32),
            n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
            _ => reply.data(value.as_bytes()),
        }
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ITEM_ID, XATTR_STATE].iter().flat_map(|n| CString::new(*n).unwrap().into_bytes_with_nul()).collect::<Vec<u8>>();
        match size {
            0 => reply.size(names.len() as u32),
            n if (n as usize) < names.len() => reply.error(Errno::ERANGE),
            _ => reply.data(&names),
        }
    }

    // Stage 1 accepts nothing that changes the folder. Refused here and not by a read-only
    // mount: on one the kernel would refuse the daemon's own announcements too.

    fn setattr(&self, _req: &Request, _ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, _size: Option<u64>, _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if announcing(_req) {
            return self.getattr(_req, _ino, None, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mkdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if announcing(_req) {
            return self.entry(_parent, _name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mknod(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if announcing(_req) {
            return self.entry(_parent, _name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        if announcing(_req) && !self.has(_parent, _name) {
            return reply.ok();
        }
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        if announcing(_req) && !self.has(_parent, _name) {
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

impl ReadOnlyFs {
    /// The entry `name` of `parent`, as the store has it now.
    fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let s = &self.shared;
        let Some(dir) = s.id(parent) else { return reply.error(Errno::ENOENT) };
        let Some(name) = name.to_str() else { return reply.error(Errno::ENOENT) };
        let row = s.store.lock().unwrap().child(Table::Items, &dir, name).ok().flatten();
        match row.filter(|r| r.placement == Placement::Placed) {
            Some(row) => {
                let ino = s.nodes.lock().unwrap().ino(&row, parent.0);
                reply.entry(&TTL, &s.attr(&row, ino), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    /// Whether the store still has `name` in `parent`.
    fn has(&self, parent: INodeNo, name: &OsStr) -> bool {
        let s = &self.shared;
        let row = s.id(parent).zip(name.to_str()).and_then(|(dir, name)| s.store.lock().unwrap().child(Table::Items, &dir, name).ok().flatten());
        row.is_some_and(|r| r.placement == Placement::Placed)
    }
}
