//! The prototype filesystem: every name is served from a backing directory, except the invented
//! entries under `cloud/`, which have a size and no object until an open fills them.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

/// Set by SIGUSR1: from then on every request waits forever, as a hung daemon would.
static HANG: AtomicBool = AtomicBool::new(false);

extern "C" fn on_usr1(_: libc::c_int) {
    HANG.store(true, Ordering::SeqCst);
}

fn now_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

#[derive(Default)]
struct State {
    paths: HashMap<u64, PathBuf>,
    inos: HashMap<PathBuf, u64>,
    next: u64,
    /// Entries that are "not downloaded": relative path to size.
    cloud: BTreeMap<PathBuf, u64>,
}

impl State {
    fn ino(&mut self, rel: &Path) -> u64 {
        if let Some(i) = self.inos.get(rel) {
            return *i;
        }
        self.next += 1;
        let i = self.next;
        self.inos.insert(rel.to_owned(), i);
        self.paths.insert(i, rel.to_owned());
        i
    }

    fn renamed(&mut self, from: &Path, to: &Path) {
        // `join` of an empty remainder would leave a trailing slash on a file's path.
        let moved_to = |p: &Path| if p == from { to.to_owned() } else { to.join(p.strip_prefix(from).unwrap()) };
        let moved: Vec<PathBuf> = self.inos.keys().filter(|p| p.starts_with(from)).cloned().collect();
        if let Some(old) = self.inos.remove(to) {
            self.paths.remove(&old);
        }
        for p in moved {
            let i = self.inos.remove(&p).unwrap();
            let n = moved_to(&p);
            self.paths.insert(i, n.clone());
            self.inos.insert(n, i);
        }
        let clouds: Vec<PathBuf> = self.cloud.keys().filter(|p| p.starts_with(from)).cloned().collect();
        for p in clouds {
            let s = self.cloud.remove(&p).unwrap();
            self.cloud.insert(moved_to(&p), s);
        }
    }
}

struct Probe {
    backing: PathBuf,
    st: Arc<Mutex<State>>,
    ttl: Duration,
    keep_cache: bool,
    direct_io: bool,
    fill_ms: u64,
    log: Option<Arc<Mutex<File>>>,
}

fn errno(e: &std::io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

fn last_errno() -> Errno {
    errno(&std::io::Error::last_os_error())
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn time(secs: i64, nsec: i64) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs.max(0) as u64, nsec as u32)
}

fn attr_of(m: &fs::Metadata, ino: u64) -> FileAttr {
    let kind = if m.is_dir() {
        FileType::Directory
    } else if m.file_type().is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    FileAttr {
        ino: INodeNo(ino),
        size: m.size(),
        blocks: m.blocks(),
        atime: time(m.atime(), m.atime_nsec()),
        mtime: time(m.mtime(), m.mtime_nsec()),
        ctime: time(m.ctime(), m.ctime_nsec()),
        crtime: UNIX_EPOCH,
        kind,
        perm: (m.mode() & 0o7777) as u16,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn cloud_attr(size: u64, ino: u64) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl Probe {
    fn gate(&self) {
        while HANG.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn log(&self, line: std::fmt::Arguments) {
        if let Some(l) = &self.log {
            let _ = writeln!(l.lock().unwrap(), "{} {}", now_ms(), line);
        }
    }

    fn rel(&self, ino: INodeNo) -> Option<PathBuf> {
        self.st.lock().unwrap().paths.get(&ino.0).cloned()
    }

    fn attr(&self, rel: &Path) -> Result<FileAttr, Errno> {
        let mut st = self.st.lock().unwrap();
        if let Some(size) = st.cloud.get(rel).copied() {
            let i = st.ino(rel);
            return Ok(cloud_attr(size, i));
        }
        let m = fs::symlink_metadata(self.backing.join(rel)).map_err(|e| errno(&e))?;
        let i = st.ino(rel);
        Ok(attr_of(&m, i))
    }

    /// Names in a directory: what the backing directory holds, then what is not downloaded.
    fn list(&self, rel: &Path) -> Result<Vec<OsString>, Errno> {
        let mut names = Vec::new();
        for e in fs::read_dir(self.backing.join(rel)).map_err(|e| errno(&e))? {
            let name = e.map_err(|e| errno(&e))?.file_name();
            if !name.as_bytes().starts_with(b".staging-") {
                names.push(name);
            }
        }
        let st = self.st.lock().unwrap();
        for (p, _) in st.cloud.range(rel.join("")..) {
            if !p.starts_with(rel) {
                break;
            }
            if p.parent() == Some(rel) {
                names.push(p.file_name().unwrap().to_owned());
            }
        }
        Ok(names)
    }

    fn open_backing(&self, rel: &Path, flags: i32) -> Result<i32, Errno> {
        // The kernel reads a page before a partial write and applies O_APPEND itself.
        let mut f = flags & !(libc::O_APPEND | libc::O_CREAT | libc::O_EXCL);
        if f & libc::O_ACCMODE == libc::O_WRONLY {
            f = (f & !libc::O_ACCMODE) | libc::O_RDWR;
        }
        let fd = unsafe { libc::open(cstr(&self.backing.join(rel)).as_ptr(), f) };
        if fd < 0 {
            return Err(last_errno());
        }
        Ok(fd)
    }

    fn fopen_flags(&self) -> FopenFlags {
        if self.direct_io {
            FopenFlags::FOPEN_DIRECT_IO
        } else if self.keep_cache {
            FopenFlags::FOPEN_KEEP_CACHE
        } else {
            FopenFlags::empty()
        }
    }
}

fn borrow(fh: FileHandle) -> std::mem::ManuallyDrop<File> {
    std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fh.0 as i32) })
}

impl Filesystem for Probe {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.add_capabilities(
            fuser::InitFlags::FUSE_DO_READDIRPLUS
                | fuser::InitFlags::FUSE_READDIRPLUS_AUTO
                | fuser::InitFlags::FUSE_PARALLEL_DIROPS,
        );
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.gate();
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        match self.attr(&dir.join(name)) {
            Ok(a) => reply.entry(&self.ttl, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.gate();
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        match self.attr(&rel) {
            Ok(a) => reply.attr(&self.ttl, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.gate();
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        let full = self.backing.join(&rel);
        self.log(format_args!("setattr {} size={size:?} mode={mode:?} fh={}", rel.display(), fh.is_some()));
        if let Some(s) = size {
            let r = match fh {
                Some(h) => borrow(h).set_len(s),
                None => File::options().write(true).open(&full).and_then(|f| f.set_len(s)),
            };
            if let Err(e) = r {
                return reply.error(errno(&e));
            }
        }
        if let Some(m) = mode {
            if let Err(e) = fs::set_permissions(&full, fs::Permissions::from_mode(m)) {
                return reply.error(errno(&e));
            }
        }
        if atime.is_some() || mtime.is_some() {
            let spec = |t: Option<TimeOrNow>| match t {
                None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
                Some(TimeOrNow::Now) => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
                Some(TimeOrNow::SpecificTime(t)) => {
                    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                    libc::timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 }
                }
            };
            let times = [spec(atime), spec(mtime)];
            unsafe {
                libc::utimensat(libc::AT_FDCWD, cstr(&full).as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
            };
        }
        match self.attr(&rel) {
            Ok(a) => reply.attr(&self.ttl, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        match fs::read_link(self.backing.join(rel)) {
            Ok(t) => reply.data(t.as_os_str().as_bytes()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, target: &Path, reply: ReplyEntry) {
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        let rel = dir.join(name);
        if let Err(e) = std::os::unix::fs::symlink(target, self.backing.join(&rel)) {
            return reply.error(errno(&e));
        }
        match self.attr(&rel) {
            Ok(a) => reply.entry(&self.ttl, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
        self.gate();
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        let rel = dir.join(name);
        self.log(format_args!("mkdir {}", rel.display()));
        if unsafe { libc::mkdir(cstr(&self.backing.join(&rel)).as_ptr(), mode) } != 0 {
            return reply.error(last_errno());
        }
        match self.attr(&rel) {
            Ok(a) => reply.entry(&self.ttl, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.gate();
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        let rel = dir.join(name);
        self.log(format_args!("unlink {}", rel.display()));
        let mut st = self.st.lock().unwrap();
        let was_cloud = st.cloud.remove(&rel).is_some();
        if !was_cloud {
            if let Err(e) = fs::remove_file(self.backing.join(&rel)) {
                return reply.error(errno(&e));
            }
        }
        st.inos.remove(&rel);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.gate();
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        let rel = dir.join(name);
        self.log(format_args!("rmdir {}", rel.display()));
        if let Err(e) = fs::remove_dir(self.backing.join(&rel)) {
            return reply.error(errno(&e));
        }
        self.st.lock().unwrap().inos.remove(&rel);
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.gate();
        let (Some(a), Some(b)) = (self.rel(parent), self.rel(newparent)) else {
            return reply.error(Errno::ENOENT);
        };
        let (from, to) = (a.join(name), b.join(newname));
        self.log(format_args!("rename {} -> {} flags={:#x}", from.display(), to.display(), flags.bits()));
        let mut st = self.st.lock().unwrap();
        // A file not downloaded moves in the table alone.
        if !st.cloud.contains_key(&from) {
            let r = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    cstr(&self.backing.join(&from)).as_ptr(),
                    libc::AT_FDCWD,
                    cstr(&self.backing.join(&to)).as_ptr(),
                    flags.bits(),
                )
            };
            if r != 0 {
                return reply.error(last_errno());
            }
        }
        st.renamed(&from, &to);
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.gate();
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        let size = self.st.lock().unwrap().cloud.get(&rel).copied();
        self.log(format_args!("open {} flags={:#x} cloud={}", rel.display(), flags.0, size.is_some()));
        let Some(size) = size else {
            return match self.open_backing(&rel, flags.0) {
                Ok(fd) => reply.opened(FileHandle(fd as u64), self.fopen_flags()),
                Err(e) => reply.error(e),
            };
        };
        // The fill: built under a staging name, renamed into place, and only then opened. The
        // request loop is not held while it runs.
        let (backing, st, fill_ms, log, fopen) =
            (self.backing.clone(), self.st.clone(), self.fill_ms, self.log.clone(), self.fopen_flags());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(fill_ms));
            let staging = backing.join(format!(".staging-{}", ino.0));
            let done = (|| -> std::io::Result<i32> {
                let f = File::create(&staging)?;
                f.write_all_at(&vec![b'c'; size as usize], 0)?;
                drop(f);
                let rel = st.lock().unwrap().paths.get(&ino.0).cloned().unwrap_or(rel);
                fs::rename(&staging, backing.join(&rel))?;
                st.lock().unwrap().cloud.remove(&rel);
                Ok(File::options().read(true).write(true).open(backing.join(&rel))?.into_raw_fd())
            })();
            if let Some(l) = &log {
                let _ = writeln!(l.lock().unwrap(), "{} filled ino={} ok={}", now_ms(), ino.0, done.is_ok());
            }
            match done {
                Ok(fd) => reply.opened(FileHandle(fd as u64), fopen),
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.gate();
        let Some(dir) = self.rel(parent) else { return reply.error(Errno::ENOENT) };
        let rel = dir.join(name);
        self.log(format_args!("create {} flags={flags:#x}", rel.display()));
        let mut f = (flags & !libc::O_APPEND) | libc::O_CREAT;
        if f & libc::O_ACCMODE == libc::O_WRONLY {
            f = (f & !libc::O_ACCMODE) | libc::O_RDWR;
        }
        let fd = unsafe { libc::open(cstr(&self.backing.join(&rel)).as_ptr(), f, mode) };
        if fd < 0 {
            return reply.error(last_errno());
        }
        match self.attr(&rel) {
            Ok(a) => reply.created(&self.ttl, &a, Generation(0), FileHandle(fd as u64), self.fopen_flags()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.gate();
        self.log(format_args!("read ino={} fh={} off={offset} len={size}", ino.0, fh.0));
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

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.gate();
        self.log(format_args!(
            "write ino={} fh={} off={offset} len={} cache={}",
            ino.0,
            fh.0,
            data.len(),
            write_flags.contains(WriteFlags::FUSE_WRITE_CACHE)
        ));
        match borrow(fh).write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        self.gate();
        self.log(format_args!("flush ino={} fh={}", ino.0, fh.0));
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.log(format_args!("release ino={} fh={}", ino.0, fh.0));
        unsafe { libc::close(fh.0 as i32) };
        reply.ok();
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        self.gate();
        self.log(format_args!("fsync ino={} fh={}", ino.0, fh.0));
        let f = borrow(fh);
        match if datasync { f.sync_data() } else { f.sync_all() } {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        self.gate();
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        let names = match self.list(&rel) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        for (i, name) in names.iter().enumerate().skip(offset as usize) {
            let Ok(a) = self.attr(&rel.join(name)) else { continue };
            if reply.add(a.ino, i as u64 + 1, a.kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        self.gate();
        let Some(rel) = self.rel(ino) else { return reply.error(Errno::ENOENT) };
        let names = match self.list(&rel) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        for (i, name) in names.iter().enumerate().skip(offset as usize) {
            let Ok(a) = self.attr(&rel.join(name)) else { continue };
            if reply.add(a.ino, i as u64 + 1, name, &self.ttl, &a, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.gate();
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(cstr(&self.backing).as_ptr(), &mut s) } != 0 {
            return reply.error(last_errno());
        }
        reply.statfs(s.f_blocks, s.f_bfree, s.f_bavail, s.f_files, s.f_ffree, s.f_bsize as u32, 255, s.f_frsize as u32);
    }
}

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut opt = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        Some(args.remove(i).into_string().unwrap())
    };
    let log = opt("--log");
    let cloud: u64 = opt("--cloud").map_or(0, |v| v.parse().unwrap());
    let cloud_size: u64 = opt("--cloud-size").map_or(4096, |v| v.parse().unwrap());
    let fill_ms: u64 = opt("--fill-ms").map_or(0, |v| v.parse().unwrap());
    let ttl_ms: u64 = opt("--ttl-ms").map_or(1000, |v| v.parse().unwrap());
    let threads: usize = opt("--threads").map_or(4, |v| v.parse().unwrap());
    let keep_cache = opt("--keep-cache").map_or(true, |v| v == "1");
    let direct_io = opt("--direct-io").is_some_and(|v| v == "1");
    let subtype = opt("--subtype");
    let [backing, mountpoint] = <[OsString; 2]>::try_from(args).expect("fuse-probe <backing> <mountpoint> [options]");
    let backing = PathBuf::from(OsString::from_vec(backing.into_vec()));

    let mut st = State { next: 1, ..State::default() };
    st.paths.insert(1, PathBuf::new());
    st.inos.insert(PathBuf::new(), 1);
    if cloud > 0 {
        fs::create_dir_all(backing.join("cloud")).unwrap();
        for i in 0..cloud {
            st.cloud.insert(PathBuf::from(format!("cloud/f{i:05}.bin")), cloud_size);
        }
    }
    unsafe { libc::signal(libc::SIGUSR1, on_usr1 as *const () as usize) };

    let probe = Probe {
        backing,
        st: Arc::new(Mutex::new(st)),
        ttl: Duration::from_millis(ttl_ms),
        keep_cache,
        direct_io,
        fill_ms,
        log: log.map(|p| Arc::new(Mutex::new(File::create(p).unwrap()))),
    };
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("konedrive".into()), MountOption::RW, MountOption::NoSuid, MountOption::NoDev];
    if let Some(s) = subtype {
        config.mount_options.push(MountOption::Subtype(s));
    }
    config.n_threads = Some(threads);
    config.clone_fd = threads > 1;
    if let Err(e) = fuser::mount(probe, &mountpoint, &config) {
        eprintln!("fuse-probe: {e}");
        std::process::exit(1);
    }
}
