//! Probe for the write phase's watcher (`docs/design/writes.md` §3):
//! what an **unprivileged** fanotify notification group can do on this
//! kernel, and exactly which events arrive, with which information records,
//! for each kind of local change.
//!
//! Run as root inside the virtme-ng VM (`tests/vm/run.sh`). Root is kept only
//! for what the helper does (a pre-content permission group) and for setting
//! the guest up; everything the daemon would do runs in a child process that
//! the probe re-executes as uid/gid 1000 with no supplementary groups and no
//! capabilities (it prints its own `CapEff` to prove it).
//!
//! Usage: watch-probe [--fs btrfs|ext4|xfs]      (default btrfs)
//!
//! This is a record, not a pass/fail suite: every line is a measurement. Only
//! what the rest depends on (the filesystem really being the one named, the
//! children running to completion) prints `FAIL` and makes the exit non-zero.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};

/// The uid/gid the daemon's side runs as. The guest shares the host's
/// `/etc/passwd`, but nothing here needs a name for it.
const USER: u32 = 1000;

/// `include/uapi/linux/fanotify.h`, spelled out so the probe does not depend
/// on which of them a given libc release happens to export.
mod fan {
    pub const CLOEXEC: u32 = 0x1;
    pub const NONBLOCK: u32 = 0x2;
    pub const CLASS_NOTIF: u32 = 0x0;
    pub const CLASS_CONTENT: u32 = 0x4;
    pub const CLASS_PRE_CONTENT: u32 = 0x8;
    pub const UNLIMITED_QUEUE: u32 = 0x10;
    pub const UNLIMITED_MARKS: u32 = 0x20;
    pub const ENABLE_AUDIT: u32 = 0x40;
    pub const REPORT_PIDFD: u32 = 0x80;
    pub const REPORT_TID: u32 = 0x100;
    pub const REPORT_FID: u32 = 0x200;
    pub const REPORT_DIR_FID: u32 = 0x400;
    pub const REPORT_NAME: u32 = 0x800;
    pub const REPORT_TARGET_FID: u32 = 0x1000;
    pub const REPORT_FD_ERROR: u32 = 0x2000;
    pub const REPORT_MNT: u32 = 0x4000;
    pub const REPORT_DFID_NAME: u32 = REPORT_DIR_FID | REPORT_NAME;
    pub const REPORT_DFID_NAME_TARGET: u32 = REPORT_DFID_NAME | REPORT_FID | REPORT_TARGET_FID;
    /// The design's group (§3.3).
    pub const DESIGN_INIT: u32 = CLASS_NOTIF | REPORT_DFID_NAME_TARGET | NONBLOCK | CLOEXEC;

    pub const MARK_ADD: u32 = 0x1;
    pub const MARK_REMOVE: u32 = 0x2;
    pub const MARK_MOUNT: u32 = 0x10;
    pub const MARK_FILESYSTEM: u32 = 0x100;

    pub const ACCESS: u64 = 0x1;
    pub const MODIFY: u64 = 0x2;
    pub const ATTRIB: u64 = 0x4;
    pub const CLOSE_WRITE: u64 = 0x8;
    pub const CLOSE_NOWRITE: u64 = 0x10;
    pub const OPEN: u64 = 0x20;
    pub const MOVED_FROM: u64 = 0x40;
    pub const MOVED_TO: u64 = 0x80;
    pub const CREATE: u64 = 0x100;
    pub const DELETE: u64 = 0x200;
    pub const DELETE_SELF: u64 = 0x400;
    pub const MOVE_SELF: u64 = 0x800;
    pub const OPEN_EXEC: u64 = 0x1000;
    pub const Q_OVERFLOW: u64 = 0x4000;
    pub const FS_ERROR: u64 = 0x8000;
    pub const OPEN_PERM: u64 = 0x10000;
    pub const ACCESS_PERM: u64 = 0x20000;
    pub const OPEN_EXEC_PERM: u64 = 0x40000;
    pub const PRE_ACCESS: u64 = 0x100000;
    pub const EVENT_ON_CHILD: u64 = 0x0800_0000;
    pub const RENAME: u64 = 0x1000_0000;
    pub const ONDIR: u64 = 0x4000_0000;

    /// The design's mask for every directory (§3.3).
    pub const DESIGN_MASK: u64 = CREATE
        | DELETE
        | RENAME
        | MOVED_FROM
        | MOVED_TO
        | CLOSE_WRITE
        | ATTRIB
        | ONDIR
        | EVENT_ON_CHILD;

    pub const NOFD: i32 = -1;
}

/// `AT_HANDLE_FID` (6.5): ask `name_to_handle_at` for a handle that only has
/// to identify the object, the way fanotify encodes one.
const AT_HANDLE_FID: i32 = 0x200;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        watchdog(Duration::from_secs(240));
        std::process::exit(child_main(&args[2..]));
    }
    watchdog(Duration::from_secs(600));
    let fs_name = args
        .iter()
        .position(|a| a == "--fs")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "btrfs".into());
    std::process::exit(root_main(&fs_name));
}

/// Prints a marker and exits if the probe wedges, so a deadlock shows up as a
/// diagnosis instead of a silent VM timeout.
fn watchdog(after: Duration) {
    thread::spawn(move || {
        thread::sleep(after);
        println!("WATCHDOG: no completion after {after:?} — the probe is wedged");
        std::process::exit(2);
    });
}

// ---------------------------------------------------------------------------
// Small syscall wrappers
// ---------------------------------------------------------------------------

fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("path without NUL")
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn ename(e: i32) -> String {
    let name = match e {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EBADF => "EBADF",
        libc::EACCES => "EACCES",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENODEV => "ENODEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ENOSPC => "ENOSPC",
        libc::ENOSYS => "ENOSYS",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ESTALE => "ESTALE",
        libc::EAGAIN => "EAGAIN",
        libc::EFAULT => "EFAULT",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::ELOOP => "ELOOP",
        _ => return format!("errno {e}"),
    };
    name.to_string()
}

fn io_err(e: &std::io::Error) -> String {
    e.raw_os_error().map(ename).unwrap_or_else(|| e.to_string())
}

fn ok_or<T>(r: Result<T, i32>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => ename(e),
    }
}

fn fan_init(flags: u32, event_f_flags: u32) -> Result<OwnedFd, i32> {
    let fd = unsafe { libc::fanotify_init(flags, event_f_flags) };
    if fd < 0 {
        Err(last_errno())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn design_group() -> OwnedFd {
    fan_init(fan::DESIGN_INIT, (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE) as u32)
        .unwrap_or_else(|e| panic!("the design's fanotify_init failed: {}", ename(e)))
}

fn fan_mark(group: &OwnedFd, flags: u32, mask: u64, path: &Path) -> Result<(), i32> {
    let c = cpath(path);
    let rc =
        unsafe { libc::fanotify_mark(group.as_raw_fd(), flags, mask, libc::AT_FDCWD, c.as_ptr()) };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

#[repr(C)]
struct RawHandle {
    bytes: u32,
    htype: i32,
    data: [u8; 128],
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Handle {
    htype: i32,
    data: Vec<u8>,
}

impl Handle {
    fn show(&self) -> String {
        let hex: String = self.data.iter().map(|b| format!("{b:02x}")).collect();
        format!("type {:#x}, {} bytes, {hex}", self.htype, self.data.len())
    }
}

/// `name_to_handle_at` on a path, not following a final symlink.
fn name_to_handle(path: &Path, flags: i32) -> Result<(Handle, i32), i32> {
    let c = cpath(path);
    let mut raw = RawHandle { bytes: 128, htype: 0, data: [0; 128] };
    let mut mount_id: libc::c_int = 0;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            libc::AT_FDCWD,
            c.as_ptr(),
            &mut raw as *mut RawHandle,
            &mut mount_id as *mut libc::c_int,
            flags,
        )
    };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok((
        Handle { htype: raw.htype, data: raw.data[..raw.bytes as usize].to_vec() },
        mount_id,
    ))
}

fn open_by_handle(mount_fd: RawFd, h: &Handle, flags: i32) -> Result<OwnedFd, i32> {
    let mut raw = RawHandle { bytes: h.data.len() as u32, htype: h.htype, data: [0; 128] };
    raw.data[..h.data.len()].copy_from_slice(&h.data);
    let rc = unsafe {
        libc::syscall(libc::SYS_open_by_handle_at, mount_fd, &mut raw as *mut RawHandle, flags)
    };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(rc as i32) })
    }
}

fn statfs_of(path: &Path) -> libc::statfs {
    let c = cpath(path);
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statfs {path:?}: {}", ename(last_errno()));
    st
}

fn fsid_of(path: &Path) -> [i32; 2] {
    let st = statfs_of(path);
    // `fsid_t`'s field is private in the libc crate; it is two ints.
    unsafe { *(&st.f_fsid as *const libc::fsid_t as *const [i32; 2]) }
}

fn show_fsid(f: [i32; 2]) -> String {
    format!("{:08x}:{:08x}", f[0] as u32, f[1] as u32)
}

fn read_sysctl(name: &str) -> String {
    fs::read_to_string(format!("/proc/sys/fs/fanotify/{name}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|e| format!("<{}>", io_err(&e)))
}

// ---------------------------------------------------------------------------
// Reading and printing events of a FID-reporting group
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Rec {
    itype: u8,
    len: u16,
    fsid: [i32; 2],
    handle: Option<Handle>,
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct Ev {
    event_len: u32,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
    recs: Vec<Rec>,
}

fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes(b[at..at + 2].try_into().unwrap())
}
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}
fn rd_i32(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

fn parse_events(buf: &[u8], out: &mut Vec<Ev>) {
    let mut off = 0;
    while off + 24 <= buf.len() {
        let event_len = rd_u32(buf, off) as usize;
        if event_len < 24 || off + event_len > buf.len() {
            break;
        }
        let metadata_len = rd_u16(buf, off + 6);
        let mask = rd_u64(buf, off + 8);
        let fd = rd_i32(buf, off + 16);
        let pid = rd_i32(buf, off + 20);
        let end = off + event_len;
        let mut recs = Vec::new();
        let mut r = off + metadata_len as usize;
        while r + 4 <= end {
            let itype = buf[r];
            let len = rd_u16(buf, r + 2) as usize;
            if len < 4 || r + len > end {
                break;
            }
            let mut rec = Rec { itype, len: len as u16, fsid: [0, 0], handle: None, name: None };
            // FID, DFID_NAME, DFID, OLD_DFID_NAME, OLD_DFID, NEW_DFID_NAME, NEW_DFID
            if matches!(itype, 1 | 2 | 3 | 10 | 11 | 12 | 13) && len >= 20 {
                rec.fsid = [rd_i32(buf, r + 4), rd_i32(buf, r + 8)];
                let hb = rd_u32(buf, r + 12) as usize;
                let ht = rd_i32(buf, r + 16);
                let hstart = r + 20;
                let hend = (hstart + hb).min(r + len);
                rec.handle = Some(Handle { htype: ht, data: buf[hstart..hend].to_vec() });
                if matches!(itype, 2 | 10 | 12) {
                    let tail = &buf[hend..r + len];
                    let nul = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
                    rec.name = Some(String::from_utf8_lossy(&tail[..nul]).into_owned());
                }
            }
            recs.push(rec);
            r += len;
        }
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        out.push(Ev { event_len: event_len as u32, metadata_len, mask, fd, pid, recs });
        off = end;
    }
}

/// Everything queued right now; a non-blocking group returns `EAGAIN` once
/// it is empty.
fn read_events(group: &OwnedFd) -> Vec<Ev> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = unsafe { libc::read(group.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = last_errno();
            if e != libc::EAGAIN {
                println!("    read() on the group failed: {}", ename(e));
            }
            break;
        }
        if n == 0 {
            break;
        }
        parse_events(&buf[..n as usize], &mut out);
    }
    out
}

/// Events are queued by the syscall that causes them (a close's by the time
/// `close()` returns), so a short settle is only a margin.
fn drain(group: &OwnedFd) -> Vec<Ev> {
    thread::sleep(Duration::from_millis(30));
    read_events(group)
}

const MASK_NAMES: [(u64, &str); 22] = [
    (fan::CREATE, "CREATE"),
    (fan::DELETE, "DELETE"),
    (fan::MOVED_FROM, "MOVED_FROM"),
    (fan::MOVED_TO, "MOVED_TO"),
    (fan::RENAME, "RENAME"),
    (fan::MODIFY, "MODIFY"),
    (fan::CLOSE_WRITE, "CLOSE_WRITE"),
    (fan::ATTRIB, "ATTRIB"),
    (fan::DELETE_SELF, "DELETE_SELF"),
    (fan::MOVE_SELF, "MOVE_SELF"),
    (fan::Q_OVERFLOW, "Q_OVERFLOW"),
    (fan::ACCESS, "ACCESS"),
    (fan::OPEN, "OPEN"),
    (fan::CLOSE_NOWRITE, "CLOSE_NOWRITE"),
    (fan::OPEN_EXEC, "OPEN_EXEC"),
    (fan::FS_ERROR, "FS_ERROR"),
    (fan::OPEN_PERM, "OPEN_PERM"),
    (fan::ACCESS_PERM, "ACCESS_PERM"),
    (fan::OPEN_EXEC_PERM, "OPEN_EXEC_PERM"),
    (fan::PRE_ACCESS, "PRE_ACCESS"),
    (fan::EVENT_ON_CHILD, "EVENT_ON_CHILD"),
    (fan::ONDIR, "ONDIR"),
];

fn mask_str(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut rest = mask;
    for (bit, name) in MASK_NAMES {
        if mask & bit != 0 {
            parts.push(name.to_string());
            rest &= !bit;
        }
    }
    if rest != 0 {
        parts.push(format!("{rest:#x}"));
    }
    if parts.is_empty() {
        "0".into()
    } else {
        parts.join("|")
    }
}

fn info_name(itype: u8) -> &'static str {
    match itype {
        1 => "FID",
        2 => "DFID_NAME",
        3 => "DFID",
        4 => "PIDFD",
        5 => "ERROR",
        6 => "RANGE",
        7 => "MNT",
        10 => "OLD_DFID_NAME",
        11 => "OLD_DFID",
        12 => "NEW_DFID_NAME",
        13 => "NEW_DFID",
        _ => "?",
    }
}

/// Turns handles into names. Directories and pre-existing objects are
/// registered by `name_to_handle_at` before anything happens — the design's
/// directory map, built unprivileged — so an event's handle printing as a
/// name *is* the check that the two encodings agree. Objects created later
/// get `#n`, with the name they were found under when the event was read.
struct Labels {
    known: HashMap<Handle, String>,
    dirs: HashMap<String, PathBuf>,
    fsid: [i32; 2],
    next: usize,
    me: i32,
}

impl Labels {
    fn new(fsid: [i32; 2]) -> Self {
        Labels {
            known: HashMap::new(),
            dirs: HashMap::new(),
            fsid,
            next: 0,
            me: std::process::id() as i32,
        }
    }

    fn add_dir(&mut self, label: &str, path: &Path) {
        let (h, _) = name_to_handle(path, 0)
            .unwrap_or_else(|e| panic!("name_to_handle_at {path:?}: {}", ename(e)));
        self.known.insert(h, label.to_string());
        self.dirs.insert(label.to_string(), path.to_path_buf());
    }

    fn move_dir(&mut self, label: &str, path: &Path) {
        self.dirs.insert(label.to_string(), path.to_path_buf());
    }

    fn add_obj(&mut self, label: &str, path: &Path) {
        let (h, _) = name_to_handle(path, 0)
            .unwrap_or_else(|e| panic!("name_to_handle_at {path:?}: {}", ename(e)));
        self.known.insert(h, label.to_string());
    }

    fn label(&mut self, h: &Handle, named: &[(String, String)]) -> String {
        if let Some(l) = self.known.get(h) {
            return l.clone();
        }
        self.next += 1;
        let mut l = format!("#{}", self.next);
        for (dir, name) in named.iter().rev() {
            if let Some(p) = self.dirs.get(dir) {
                if let Ok((h2, _)) = name_to_handle(&p.join(name), 0) {
                    if &h2 == h {
                        l = format!("#{}={}", self.next, name);
                        break;
                    }
                }
            }
        }
        self.known.insert(h.clone(), l.clone());
        l
    }

    fn fmt(&mut self, ev: &Ev) -> String {
        let pid = if ev.pid == self.me {
            "self".to_string()
        } else {
            ev.pid.to_string()
        };
        let mut s = format!("{} pid={pid}", mask_str(ev.mask));
        if ev.fd != fan::NOFD {
            s += &format!(" fd={}", ev.fd);
        }
        let named: Vec<(String, String)> = ev
            .recs
            .iter()
            .filter_map(|r| {
                let name = r.name.clone()?;
                let dir = self.known.get(r.handle.as_ref()?)?.clone();
                Some((dir, name))
            })
            .collect();
        for r in &ev.recs {
            let tag = info_name(r.itype);
            match &r.handle {
                Some(h) => {
                    let label = self.label(h, &named);
                    let fsid = if r.fsid != self.fsid {
                        format!("[fsid {} != statfs]", show_fsid(r.fsid))
                    } else {
                        String::new()
                    };
                    match &r.name {
                        Some(n) => s += &format!(" {tag}({label},{n:?}){fsid}"),
                        None => s += &format!(" {tag}({label}){fsid}"),
                    }
                }
                None => s += &format!(" {tag}(len {})", r.len),
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Root side
// ---------------------------------------------------------------------------

const FILESYSTEMS: [(&str, i64); 3] =
    [("btrfs", 0x9123_683E), ("ext4", 0x0000_EF53), ("xfs", 0x5846_5342)];

fn root_main(fs_name: &str) -> i32 {
    let mut failures = 0;
    let mnt = PathBuf::from(format!("/mnt/{fs_name}"));
    let Some((_, magic)) = FILESYSTEMS.iter().find(|(n, _)| *n == fs_name) else {
        println!("FAIL unknown --fs {fs_name}");
        return 1;
    };
    if unsafe { libc::getuid() } != 0 {
        println!("FAIL the probe must start as root, inside the VM");
        return 1;
    }
    let got = statfs_of(&mnt).f_type as i64;
    if got != *magic {
        println!(
            "FAIL {mnt:?} reports f_type {got:#x}, expected {magic:#x} — the mount did not happen"
        );
        return 1;
    }
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!("watch-probe: kernel {}, filesystem {fs_name} at {mnt:?}", release.trim());

    println!("== /proc/sys/fs/fanotify (read as root)");
    let mut names: Vec<String> = fs::read_dir("/proc/sys/fs/fanotify")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    for n in &names {
        println!("  {n} = {}", read_sysctl(n));
    }

    let w = mnt.join("wprobe");
    let _ = fs::remove_dir_all(&w);
    mkdir_owned(&w);

    root_facts(&w);

    // --- what an unprivileged process may do at all
    let flags_dir = w.join("flags");
    mkdir_owned(&flags_dir);
    let rootonly = w.join("rootonly");
    fs::create_dir(&rootonly).unwrap();
    fs::set_permissions(&rootonly, fs::Permissions::from_mode(0o700)).unwrap();
    let tmpfs_dir = PathBuf::from("/mnt/wprobe-tmpfs");
    let _ = fs::remove_dir_all(&tmpfs_dir);
    mkdir_owned(&tmpfs_dir);
    let subvol = if fs_name == "btrfs" {
        let sv = w.join("subvol");
        let out = Command::new("btrfs").arg("subvolume").arg("create").arg(&sv).output();
        match out {
            Ok(o) if o.status.success() => {
                chown_user(&sv);
                sv.to_string_lossy().into_owned()
            }
            other => {
                println!("  (btrfs subvolume create failed: {other:?})");
                "-".into()
            }
        }
    } else {
        "-".into()
    };
    let fsroot = mnt.to_string_lossy().into_owned();
    failures += run_child_inherit(
        "flags",
        &[
            &flags_dir.to_string_lossy(),
            &rootonly.to_string_lossy(),
            &fsroot,
            &tmpfs_dir.to_string_lossy(),
            &subvol,
        ],
    );

    // --- the event matrix, alone
    let m1 = w.join("m1");
    prepare_matrix(&m1);
    println!("== event matrix: the design's group alone (uid {USER})");
    let (ok1, solo) = run_child_captured("matrix", &[&m1.to_string_lossy()]);
    print!("{solo}");
    if !ok1 {
        println!("FAIL the matrix child did not finish cleanly");
        failures += 1;
    }

    // --- the same matrix while a root pre-content group marks the same directories
    let helper = HelperSim::start();
    let m2 = w.join("m2");
    prepare_matrix(&m2);
    for d in [m2.join("tree"), m2.join("tree/A"), m2.join("tree/B")] {
        helper.mark(&d);
    }
    println!(
        "== event matrix again, while a root FAN_CLASS_PRE_CONTENT group holds FAN_OPEN_PERM|FAN_EVENT_ON_CHILD \
         marks on tree, A and B and allows every open"
    );
    let (ok2, coexist) = run_child_captured("matrix", &[&m2.to_string_lossy()]);
    if !ok2 {
        println!("FAIL the second matrix child did not finish cleanly");
        failures += 1;
    }
    compare_runs(&solo, &coexist);
    println!("  pre-content events the root group answered: {}", helper.answered());

    // --- fills and denials through the root group's event descriptors
    let fill = w.join("fill");
    mkdir_owned(&fill);
    for name in FILL_FILES {
        let p = fill.join(name);
        fs::write(&p, vec![b'a'; 8192]).unwrap();
        chown_user(&p);
    }
    helper.mark(&fill);
    println!(
        "== writes through a pre-content event descriptor (the helper's fill path), seen by the unprivileged group"
    );
    failures += run_child_inherit("fill", &[&fill.to_string_lossy()]);
    for note in helper.notes() {
        println!("  root group: {note}");
    }
    drop(helper);

    // --- queue overflow
    let ovf = w.join("overflow");
    mkdir_owned(&ovf);
    println!("== queue overflow (uid {USER})");
    failures += run_child_inherit("overflow", &[&ovf.to_string_lossy()]);

    // --- per-user mark limit, lowered for the test and put back
    let ml = w.join("marklimit");
    mkdir_owned(&ml);
    let saved = read_sysctl("max_user_marks");
    let lowered = "40";
    match fs::write("/proc/sys/fs/fanotify/max_user_marks", lowered) {
        Ok(()) => {
            println!("== per-user mark limit: max_user_marks lowered from {saved} to {lowered} for this step");
            failures += run_child_inherit("marklimit", &[&ml.to_string_lossy()]);
            let _ = fs::write("/proc/sys/fs/fanotify/max_user_marks", &saved);
            println!("  max_user_marks restored to {}", read_sysctl("max_user_marks"));
        }
        Err(e) => println!("== per-user mark limit: cannot lower max_user_marks: {}", io_err(&e)),
    }

    println!("failures={failures}");
    if failures == 0 {
        0
    } else {
        1
    }
}

fn chown_user(p: &Path) {
    std::os::unix::fs::lchown(p, Some(USER), Some(USER))
        .unwrap_or_else(|e| panic!("chown {p:?}: {e}"));
}

fn mkdir_owned(p: &Path) {
    fs::create_dir_all(p).unwrap_or_else(|e| panic!("mkdir {p:?}: {e}"));
    chown_user(p);
}

fn prepare_matrix(base: &Path) {
    let _ = fs::remove_dir_all(base);
    for d in ["", "tree", "tree/A", "tree/B", "out"] {
        mkdir_owned(&base.join(d));
    }
}

/// What root can do that the daemon cannot, and the design's premise that FID
/// reporting is refused to the permission classes.
fn root_facts(w: &Path) {
    println!("== fanotify_init as root (the helper's side)");
    let cases: [(&str, u32); 6] = [
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_FID", fan::CLASS_PRE_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_DFID_NAME", fan::CLASS_PRE_CONTENT | fan::REPORT_DFID_NAME),
        ("FAN_CLASS_CONTENT | FAN_REPORT_FID", fan::CLASS_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT (the helper's, no FID)", fan::CLASS_PRE_CONTENT | fan::UNLIMITED_QUEUE | fan::UNLIMITED_MARKS),
        ("the design's notification group + FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS", fan::DESIGN_INIT | fan::UNLIMITED_QUEUE | fan::UNLIMITED_MARKS),
        ("the design's notification group + FAN_REPORT_PIDFD", fan::DESIGN_INIT | fan::REPORT_PIDFD),
    ];
    for (label, flags) in cases {
        let ev = (libc::O_RDONLY | libc::O_CLOEXEC) as u32;
        println!("  {label}: {}", ok_or(fan_init(flags, ev)));
    }
    // Control for the unprivileged refusal below.
    let dir = File::open(w).unwrap();
    match name_to_handle(w, 0) {
        Ok((h, _)) => {
            let r = open_by_handle(dir.as_raw_fd(), &h, libc::O_RDONLY | libc::O_DIRECTORY);
            println!("  open_by_handle_at as root (control): {}", ok_or(r));
        }
        Err(e) => println!("  name_to_handle_at as root: {}", ename(e)),
    }
}

fn child_command(mode: &str, args: &[&str]) -> Command {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.arg("--child").arg(mode).args(args);
    // std calls setgroups(0) before setuid when the parent is root, so the
    // child keeps no supplementary group; the kernel clears every capability
    // on the switch from uid 0 to a non-zero uid.
    cmd.gid(USER).uid(USER);
    cmd
}

fn wait_bounded(child: &mut std::process::Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                println!("  (child killed after {limit:?})");
                return false;
            }
        }
    }
}

fn run_child_inherit(mode: &str, args: &[&str]) -> u32 {
    let mut child = child_command(mode, args).spawn().expect("spawn child");
    if wait_bounded(&mut child, Duration::from_secs(200)) {
        0
    } else {
        println!("FAIL child {mode} did not finish cleanly");
        1
    }
}

fn run_child_captured(mode: &str, args: &[&str]) -> (bool, String) {
    let mut child = child_command(mode, args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let ok = wait_bounded(&mut child, Duration::from_secs(200));
    (ok, reader.join().unwrap_or_default())
}

/// An `O_TMPFILE`'s events carry the pseudo-name `#<inode number>`, which
/// differs between two runs for no reason that matters here.
fn normalize_tmpfile_names(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("\"#") {
        out.push_str(&rest[..i + 2]);
        let tail = &rest[i + 2..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && tail[digits..].starts_with('"') {
            out.push_str("ino");
            rest = &tail[digits..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn compare_runs(a: &str, b: &str) {
    let la: Vec<String> = a.lines().map(normalize_tmpfile_names).collect();
    let lb: Vec<String> = b.lines().map(normalize_tmpfile_names).collect();
    if la == lb {
        println!(
            "  identical to the run without the pre-content group: {} lines, every event the same \
             (an O_TMPFILE's \"#<inode>\" pseudo-name aside)",
            la.len()
        );
        return;
    }
    println!("  DIFFERS from the run without the pre-content group:");
    for i in 0..la.len().max(lb.len()) {
        let x = la.get(i).map(String::as_str).unwrap_or("<none>");
        let y = lb.get(i).map(String::as_str).unwrap_or("<none>");
        if x != y {
            println!("    line {i}: alone:   {x}");
            println!("    line {i}: coexist: {y}");
        }
    }
}

const FILL_FILES: [&str; 7] = [
    "fill-control",
    "fill-write",
    "fill-trunc",
    "fill-times",
    "fill-xattr",
    "fill-punch",
    "fill-own",
];

/// A minimal stand-in for `konedrive-helper`: a pre-content group set up as
/// the helper sets its own up, marking directories with `FAN_OPEN_PERM |
/// FAN_EVENT_ON_CHILD`, answering from its own thread. For a file named
/// `fill-*` it first does one kind of write through the event descriptor,
/// the way a fill writes; `denyme*` is denied.
struct HelperSim {
    group: Arc<Fanotify>,
    stop: Arc<AtomicBool>,
    notes: Arc<Mutex<Vec<String>>>,
    count: Arc<Mutex<u64>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HelperSim {
    fn start() -> Self {
        let group = Arc::new(
            Fanotify::init(
                InitFlags::FAN_CLASS_PRE_CONTENT
                    | InitFlags::FAN_CLOEXEC
                    | InitFlags::FAN_UNLIMITED_QUEUE
                    | InitFlags::FAN_UNLIMITED_MARKS
                    | InitFlags::FAN_NONBLOCK,
                EventFFlags::O_RDWR
                    | EventFFlags::O_LARGEFILE
                    | EventFFlags::O_CLOEXEC
                    | EventFFlags::O_NONBLOCK,
            )
            .expect("pre-content fanotify_init as root"),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(Mutex::new(0u64));
        let (g, s, n, c) = (group.clone(), stop.clone(), notes.clone(), count.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match g.read_events() {
                    Ok(events) if events.is_empty() => thread::sleep(Duration::from_millis(2)),
                    Ok(events) => {
                        for event in events {
                            let Some(fd) = event.fd() else {
                                n.lock().unwrap().push("overflow".into());
                                continue;
                            };
                            let path = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
                                .unwrap_or_default();
                            let name = path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let mut answer = Response::FAN_ALLOW;
                            if let Some(note) = fill_through(fd.as_raw_fd(), &name) {
                                n.lock().unwrap().push(format!("{name}: {note}"));
                            }
                            if name.starts_with("denyme") {
                                answer = Response::FAN_DENY;
                                n.lock().unwrap().push(format!("{name}: FAN_DENY"));
                            }
                            *c.lock().unwrap() += 1;
                            if let Err(e) = g.write_response(FanotifyResponse::new(fd, answer)) {
                                n.lock().unwrap().push(format!("{name}: write_response {e}"));
                            }
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => thread::sleep(Duration::from_millis(2)),
                    Err(e) => {
                        n.lock().unwrap().push(format!("read_events: {e}"));
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        });
        HelperSim { group, stop, notes, count, thread: Some(thread) }
    }

    fn mark(&self, dir: &Path) {
        let fd = File::open(dir).unwrap();
        self.group
            .mark(
                MarkFlags::FAN_MARK_ADD,
                MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
                fd.as_fd(),
                None::<&Path>,
            )
            .unwrap_or_else(|e| panic!("pre-content mark {dir:?}: {e}"));
    }

    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }

    fn answered(&self) -> u64 {
        *self.count.lock().unwrap()
    }
}

impl Drop for HelperSim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One kind of write through a pre-content event descriptor, chosen by name.
fn fill_through(fd: RawFd, name: &str) -> Option<String> {
    let rc = |r: libc::c_int| if r < 0 { ename(last_errno()) } else { "ok".into() };
    Some(match name {
        "fill-write" => {
            let n = unsafe { libc::pwrite(fd, b"HELLO".as_ptr().cast(), 5, 0) };
            format!("pwrite 5 bytes through the event descriptor: {}", rc(n as i32))
        }
        "fill-trunc" => format!(
            "ftruncate(16384) through the event descriptor: {}",
            rc(unsafe { libc::ftruncate(fd, 16384) })
        ),
        "fill-times" => {
            let ts = [
                libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
                libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
            ];
            format!(
                "futimens through the event descriptor: {}",
                rc(unsafe { libc::futimens(fd, ts.as_ptr()) })
            )
        }
        "fill-xattr" => {
            let key = CString::new("user.konedrive.state").unwrap();
            let r = unsafe {
                libc::fsetxattr(fd, key.as_ptr(), b"hydrated".as_ptr().cast(), 8, 0)
            };
            format!("fsetxattr user.konedrive.state through the event descriptor: {}", rc(r))
        }
        "fill-punch" => format!(
            "fallocate(PUNCH_HOLE|KEEP_SIZE, 0, 4096) through the event descriptor: {}",
            rc(unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    0,
                    4096,
                )
            })
        ),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Unprivileged side
// ---------------------------------------------------------------------------

fn child_main(args: &[String]) -> i32 {
    let mode = args.first().map(String::as_str).unwrap_or("");
    let p = |i: usize| PathBuf::from(&args[i]);
    match mode {
        "flags" => child_flags(&p(1), &p(2), &p(3), &p(4), &args[5]),
        "matrix" => child_matrix(&p(1)),
        "fill" => child_fill(&p(1)),
        "overflow" => child_overflow(&p(1)),
        "marklimit" => child_marklimit(&p(1)),
        _ => {
            println!("unknown child mode {mode:?}");
            1
        }
    }
}

fn identity() -> String {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find(|l| l.starts_with(k))
            .map(|l| l[k.len()..].split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    format!(
        "Uid {} / Gid {} / Groups [{}] / CapEff {} / CapPrm {}",
        field("Uid:"),
        field("Gid:"),
        field("Groups:"),
        field("CapEff:"),
        field("CapPrm:")
    )
}

fn child_flags(own: &Path, rootonly: &Path, fsroot: &Path, tmpfs: &Path, subvol: &str) -> i32 {
    println!("== the unprivileged child: {}", identity());

    println!("== fanotify_init, unprivileged (event_f_flags O_RDONLY|O_CLOEXEC)");
    let d = fan::DESIGN_INIT;
    let cases: [(&str, u32); 16] = [
        ("FAN_CLASS_NOTIF, no FID reporting", fan::CLASS_NOTIF),
        ("FAN_REPORT_FID", fan::REPORT_FID),
        ("FAN_REPORT_DIR_FID", fan::REPORT_DIR_FID),
        ("FAN_REPORT_DFID_NAME", fan::REPORT_DFID_NAME),
        ("FAN_REPORT_DFID_NAME | FAN_REPORT_FID", fan::REPORT_DFID_NAME | fan::REPORT_FID),
        ("FAN_REPORT_DFID_NAME_TARGET | FAN_NONBLOCK | FAN_CLOEXEC (the design's)", d),
        ("the design's + FAN_UNLIMITED_QUEUE", d | fan::UNLIMITED_QUEUE),
        ("the design's + FAN_UNLIMITED_MARKS", d | fan::UNLIMITED_MARKS),
        ("the design's + FAN_REPORT_TID", d | fan::REPORT_TID),
        ("the design's + FAN_REPORT_PIDFD", d | fan::REPORT_PIDFD),
        ("the design's + FAN_ENABLE_AUDIT", d | fan::ENABLE_AUDIT),
        ("the design's + FAN_REPORT_FD_ERROR", d | fan::REPORT_FD_ERROR),
        ("FAN_REPORT_MNT", fan::REPORT_MNT),
        ("FAN_CLASS_CONTENT | FAN_REPORT_FID", fan::CLASS_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_FID", fan::CLASS_PRE_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT", fan::CLASS_PRE_CONTENT),
    ];
    for (label, flags) in cases {
        let r = fan_init(flags, (libc::O_RDONLY | libc::O_CLOEXEC) as u32);
        println!("  {label}: {}", ok_or(r));
    }

    {
        println!("== fanotify_mark, unprivileged, in the design's group");
        let g = design_group();
        let gx = design_group();
        let full = fan::DESIGN_MASK | fan::MODIFY | fan::DELETE_SELF | fan::MOVE_SELF;
        println!(
            "  inode mark, own directory, the design's mask: {}",
            ok_or(fan_mark(&g, fan::MARK_ADD, fan::DESIGN_MASK, own))
        );
        println!(
            "  the same, + FAN_MODIFY | FAN_DELETE_SELF | FAN_MOVE_SELF: {}",
            ok_or(fan_mark(&g, fan::MARK_ADD, full, own))
        );
        println!(
            "  FAN_MARK_MOUNT on own directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD | fan::MARK_MOUNT, fan::CREATE, own))
        );
        println!(
            "  FAN_MARK_FILESYSTEM on own directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD | fan::MARK_FILESYSTEM, fan::CREATE, own))
        );
        println!(
            "  inode mark on a root-owned 0755 directory ({}): {}",
            fsroot.display(),
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::DESIGN_MASK, fsroot))
        );
        println!(
            "  inode mark on a root-owned 0700 directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::DESIGN_MASK, rootonly))
        );
        println!(
            "  FAN_OPEN_PERM in a notification group: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::OPEN_PERM | fan::EVENT_ON_CHILD, own))
        );
        let gfid = fan_init(fan::REPORT_FID | fan::NONBLOCK, libc::O_RDONLY as u32).unwrap();
        println!(
            "  FAN_RENAME in a FAN_REPORT_FID-only group: {}",
            ok_or(fan_mark(&gfid, fan::MARK_ADD, fan::RENAME, own))
        );
        let gdfid = fan_init(fan::REPORT_DIR_FID | fan::NONBLOCK, libc::O_RDONLY as u32).unwrap();
        println!(
            "  FAN_RENAME in a FAN_REPORT_DIR_FID group (no names): {}",
            ok_or(fan_mark(&gdfid, fan::MARK_ADD, fan::RENAME, own))
        );
        println!(
            "  FAN_CREATE|FAN_ONDIR|FAN_EVENT_ON_CHILD in a FAN_REPORT_FID-only group: {}",
            ok_or(fan_mark(&gfid, fan::MARK_ADD, fan::CREATE | fan::ONDIR | fan::EVENT_ON_CHILD, own))
        );

        println!("== handles, unprivileged");
        let (hdir, mnt_id) = match name_to_handle(own, 0) {
            Ok(x) => x,
            Err(e) => {
                println!("  name_to_handle_at(own directory): {}", ename(e));
                return 1;
            }
        };
        println!("  name_to_handle_at(own directory): OK, {}, mount id {mnt_id}", hdir.show());
        match name_to_handle(own, AT_HANDLE_FID) {
            Ok((h, _)) => println!(
                "  name_to_handle_at(own directory, AT_HANDLE_FID): OK, {} ({} the plain handle)",
                h.show(),
                if h == hdir { "equal to" } else { "DIFFERENT from" }
            ),
            Err(e) => println!("  name_to_handle_at(own directory, AT_HANDLE_FID): {}", ename(e)),
        }
        let _ = read_events(&g);
        let file = own.join("probe-file");
        File::create(&file).unwrap();
        let (hfile, _) = name_to_handle(&file, 0).unwrap();
        let events = drain(&g);
        let statfs_fsid = fsid_of(own);
        println!("  statfs(own directory).f_fsid = {}", show_fsid(statfs_fsid));
        for ev in &events {
            println!(
                "  event {} pid={} (own pid {}), fd={}, {} info record(s):",
                mask_str(ev.mask),
                ev.pid,
                std::process::id(),
                ev.fd,
                ev.recs.len()
            );
            for r in &ev.recs {
                let Some(h) = &r.handle else {
                    println!("    {} (len {})", info_name(r.itype), r.len);
                    continue;
                };
                let against = if r.name.is_some() { &hdir } else { &hfile };
                println!(
                    "    {} fsid {} ({} statfs) handle {} name {:?} — {} name_to_handle_at({})",
                    info_name(r.itype),
                    show_fsid(r.fsid),
                    if r.fsid == statfs_fsid { "==" } else { "!=" },
                    h.show(),
                    r.name.as_deref().unwrap_or("-"),
                    if h == against { "EQUAL to" } else { "DIFFERENT from" },
                    if r.name.is_some() { "the directory" } else { "the file" }
                );
            }
        }
        let dirfd = File::open(own).unwrap();
        println!(
            "  open_by_handle_at(file handle, O_RDONLY), unprivileged: {}",
            ok_or(open_by_handle(dirfd.as_raw_fd(), &hfile, libc::O_RDONLY))
        );
        println!(
            "  open_by_handle_at(directory handle, O_RDONLY|O_DIRECTORY), unprivileged: {}",
            ok_or(open_by_handle(dirfd.as_raw_fd(), &hdir, libc::O_RDONLY | libc::O_DIRECTORY))
        );
        let sub = own.join("sub");
        fs::create_dir(&sub).unwrap();
        let (h1, _) = name_to_handle(&sub, 0).unwrap();
        fs::rename(&sub, own.join("sub-renamed")).unwrap();
        let (h2, _) = name_to_handle(&own.join("sub-renamed"), 0).unwrap();
        println!(
            "  a directory's handle before and after a rename: {}",
            if h1 == h2 { "equal" } else { "DIFFERENT" }
        );
        let _ = read_events(&g);

        println!("== tmpfs ({}), unprivileged", tmpfs.display());
        let gt = design_group();
        let r = fan_mark(&gt, fan::MARK_ADD, fan::DESIGN_MASK, tmpfs);
        println!("  inode mark with the design's group: {}", ok_or(r));
        match name_to_handle(tmpfs, 0) {
            Ok((h, _)) => {
                println!("  name_to_handle_at: OK, {}", h.show());
                if r.is_ok() {
                    File::create(tmpfs.join("f")).unwrap();
                    let fsid = fsid_of(tmpfs);
                    for ev in drain(&gt) {
                        let rec = ev.recs.iter().find(|r| r.name.is_some());
                        println!(
                            "  event {} — DFID {} name_to_handle_at, fsid {} ({} statfs)",
                            mask_str(ev.mask),
                            if rec.and_then(|r| r.handle.as_ref()) == Some(&h) { "EQUAL to" } else { "DIFFERENT from" },
                            rec.map(|r| show_fsid(r.fsid)).unwrap_or_default(),
                            if rec.map(|r| r.fsid) == Some(fsid) { "==" } else { "!=" },
                        );
                    }
                }
            }
            Err(e) => println!("  name_to_handle_at: {}", ename(e)),
        }

        if subvol != "-" {
            let sv = Path::new(subvol);
            println!("== a btrfs subvolume inside the filesystem, unprivileged");
            let gs = design_group();
            println!(
                "  mark a directory on the top-level subvolume: {}",
                ok_or(fan_mark(&gs, fan::MARK_ADD, fan::DESIGN_MASK, own))
            );
            let r = fan_mark(&gs, fan::MARK_ADD, fan::DESIGN_MASK, sv);
            println!("  mark the subvolume's root, in the same group: {}", ok_or(r));
            println!(
                "  statfs f_fsid: top-level {} / subvolume {}",
                show_fsid(fsid_of(own)),
                show_fsid(fsid_of(sv))
            );
            let alone = design_group();
            let r2 = fan_mark(&alone, fan::MARK_ADD, fan::DESIGN_MASK, sv);
            println!("  mark the subvolume's root in a group of its own: {}", ok_or(r2));
            if r2.is_ok() {
                let (h, _) = name_to_handle(sv, 0).unwrap();
                File::create(sv.join("in-own-group")).unwrap();
                for ev in drain(&alone) {
                    let rec = ev.recs.iter().find(|r| r.name.is_some()).unwrap();
                    println!(
                        "  event {} name {:?}: fsid {} ({} statfs(subvolume)); DFID {} name_to_handle_at(subvolume root)",
                        mask_str(ev.mask),
                        rec.name.as_deref().unwrap_or(""),
                        show_fsid(rec.fsid),
                        if rec.fsid == fsid_of(sv) { "==" } else { "!=" },
                        if rec.handle.as_ref() == Some(&h) { "EQUAL to" } else { "not equal to" }
                    );
                }
            }
            if r.is_ok() {
                let (h, _) = name_to_handle(sv, 0).unwrap();
                File::create(sv.join("f")).unwrap();
                File::create(own.join("g")).unwrap();
                for ev in drain(&gs) {
                    let rec = ev.recs.iter().find(|r| r.name.is_some()).unwrap();
                    println!(
                        "  event {} name {:?}: fsid {}; DFID {} name_to_handle_at(subvolume root)",
                        mask_str(ev.mask),
                        rec.name.as_deref().unwrap_or(""),
                        show_fsid(rec.fsid),
                        if rec.handle.as_ref() == Some(&h) { "EQUAL to" } else { "not equal to" }
                    );
                }
            }
        }
    }

    println!("== per-user group limit (max_user_groups = {})", read_sysctl("max_user_groups"));
    let mut held = Vec::new();
    let err = loop {
        match fan_init(fan::DESIGN_INIT, libc::O_RDONLY as u32) {
            Ok(fd) => held.push(fd),
            Err(e) => break Some(e),
        }
        if held.len() >= 4096 {
            break None;
        }
    };
    println!(
        "  {} groups created, then {}",
        held.len(),
        err.map(ename).unwrap_or_else(|| "no refusal up to 4096".into())
    );
    drop(held);
    0
}

/// The matrix of operations, each followed by the events it produced.
struct Matrix {
    group: OwnedFd,
    labels: Labels,
}

impl Matrix {
    fn step(&mut self, name: &str, f: impl FnOnce() -> Result<(), String>) {
        let outcome = f();
        let events = drain(&self.group);
        match outcome {
            Ok(()) => println!("op: {name}"),
            Err(e) => println!("op: {name}  [op failed: {e}]"),
        }
        if events.is_empty() {
            println!("    (no events)");
        }
        for ev in &events {
            println!("    {}", self.labels.fmt(ev));
        }
    }
}

fn io<T>(r: std::io::Result<T>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {}", io_err(&e)))
}

fn write_file(p: &Path, content: &[u8]) -> Result<(), String> {
    io(fs::write(p, content), "write")
}

fn child_matrix(base: &Path) -> i32 {
    let tree = base.join("tree");
    let a = tree.join("A");
    let b = tree.join("B");
    let out = base.join("out");

    // Everything that exists before the marks: created first, so none of it
    // is an event.
    let pre_a = ["doc", "rmme", "chm", "h", "outfile", "xa", "touchme"];
    for n in pre_a {
        fs::write(a.join(n), b"v1").unwrap();
    }
    fs::create_dir(a.join("outdir")).unwrap();
    fs::write(a.join("outdir/inner"), b"x").unwrap();
    fs::write(out.join("in-file"), b"x").unwrap();
    fs::create_dir(out.join("in-dir")).unwrap();
    fs::write(out.join("in-dir/inner"), b"x").unwrap();
    fs::write(out.join("lnk-src"), b"x").unwrap();

    let group = design_group();
    let all = fan::DESIGN_MASK | fan::MODIFY;
    fan_mark(&group, fan::MARK_ADD, all | fan::DELETE_SELF | fan::MOVE_SELF, &tree).unwrap();
    fan_mark(&group, fan::MARK_ADD, all, &a).unwrap();
    fan_mark(&group, fan::MARK_ADD, all, &b).unwrap();
    println!(
        "group: FAN_CLASS_NOTIF|FAN_REPORT_DFID_NAME_TARGET; marks: tree = design mask|MODIFY|DELETE_SELF|MOVE_SELF, \
         A and B = design mask|MODIFY; out is not marked"
    );
    println!("labels: handles from name_to_handle_at taken before any event; #n = an object first seen in an event");

    let mut labels = Labels::new(fsid_of(&tree));
    labels.add_dir("tree", &tree);
    labels.add_dir("A", &a);
    labels.add_dir("B", &b);
    labels.add_dir("out", &out);
    for n in pre_a {
        labels.add_obj(n, &a.join(n));
    }
    labels.add_obj("outdir", &a.join("outdir"));
    labels.add_obj("in-file", &out.join("in-file"));
    labels.add_obj("in-dir", &out.join("in-dir"));
    labels.add_obj("lnk-src", &out.join("lnk-src"));
    let mut m = Matrix { group, labels };
    let pre = drain(&m.group);
    println!("before any operation: {} event(s)", pre.len());

    let j = |p: &Path, n: &str| p.join(n);

    m.step("create a file: open(A/new, O_CREAT|O_EXCL|O_WRONLY), close", || {
        io(fs::OpenOptions::new().write(true).create_new(true).open(j(&a, "new")), "open").map(drop)
    });
    m.step("mkdir A/d1", || io(fs::create_dir(j(&a, "d1")), "mkdir"));
    let mut held: Option<File> = None;
    m.step("open A/new O_WRONLY and write 3 bytes, descriptor still open", || {
        let mut f = io(fs::OpenOptions::new().write(true).open(j(&a, "new")), "open")?;
        io(f.write_all(b"abc"), "write")?;
        held = Some(f);
        Ok(())
    });
    m.step("... then close it", || {
        drop(held.take());
        Ok(())
    });
    m.step("write+close in one go: open A/new O_WRONLY|O_APPEND, write, close", || {
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&a, "new")), "open")?;
        io(f.write_all(b"def"), "write")
    });
    m.step("truncate(A/new, 0) by path", || {
        let c = cpath(&j(&a, "new"));
        if unsafe { libc::truncate(c.as_ptr(), 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("rename within a directory: A/new -> A/renamed", || {
        io(fs::rename(j(&a, "new"), j(&a, "renamed")), "rename")
    });
    m.step("rename a directory within a directory: A/d1 -> A/d1r", || {
        io(fs::rename(j(&a, "d1"), j(&a, "d1r")), "rename")
    });
    m.step("rename across marked directories: A/renamed -> B/renamed", || {
        io(fs::rename(j(&a, "renamed"), j(&b, "renamed")), "rename")
    });
    m.step("move a file into the tree: out/in-file -> A/in-file", || {
        io(fs::rename(j(&out, "in-file"), j(&a, "in-file")), "rename")
    });
    m.step("move a directory into the tree: out/in-dir -> A/in-dir", || {
        io(fs::rename(j(&out, "in-dir"), j(&a, "in-dir")), "rename")
    });
    m.step("create a file inside the moved-in directory A/in-dir (which nobody marked)", || {
        write_file(&a.join("in-dir/fresh"), b"x")
    });
    m.step("move a file out of the tree: A/outfile -> out/outfile", || {
        io(fs::rename(j(&a, "outfile"), j(&out, "outfile")), "rename")
    });
    m.step("move a directory out of the tree: A/outdir -> out/outdir", || {
        io(fs::rename(j(&a, "outdir"), j(&out, "outdir")), "rename")
    });
    m.step("unlink A/rmme", || io(fs::remove_file(j(&a, "rmme")), "unlink"));
    m.step("rmdir A/d1r", || io(fs::remove_dir(j(&a, "d1r")), "rmdir"));
    m.step("chmod A/chm 0600", || {
        io(fs::set_permissions(j(&a, "chm"), fs::Permissions::from_mode(0o600)), "chmod")
    });
    m.step("chmod a marked directory itself: A 0700", || {
        io(fs::set_permissions(&a, fs::Permissions::from_mode(0o700)), "chmod")
    });
    m.step("chmod an unmarked child directory: A/in-dir 0700", || {
        io(fs::set_permissions(j(&a, "in-dir"), fs::Permissions::from_mode(0o700)), "chmod")
    });
    m.step("touch A/touchme (utimensat, both times to now)", || {
        let c = cpath(&j(&a, "touchme"));
        if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("setxattr A/xa user.test", || {
        let c = cpath(&j(&a, "xa"));
        let key = CString::new("user.test").unwrap();
        if unsafe { libc::setxattr(c.as_ptr(), key.as_ptr(), b"1".as_ptr().cast(), 1, 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("safe save: write A/.doc.tmp, close, rename it over A/doc", || {
        write_file(&j(&a, ".doc.tmp"), b"v2")?;
        io(fs::rename(j(&a, ".doc.tmp"), j(&a, "doc")), "rename")
    });
    m.step("backup-rename save: A/doc -> A/doc~, write a new A/doc, close, unlink A/doc~", || {
        io(fs::rename(j(&a, "doc"), j(&a, "doc~")), "rename")?;
        write_file(&j(&a, "doc"), b"v3")?;
        io(fs::remove_file(j(&a, "doc~")), "unlink")
    });
    m.step("hard link within the tree: A/h -> A/h2", || {
        io(fs::hard_link(j(&a, "h"), j(&a, "h2")), "link")
    });
    m.step("hard link into the tree: out/lnk-src -> A/lnk", || {
        io(fs::hard_link(j(&out, "lnk-src"), j(&a, "lnk")), "link")
    });
    m.step("hard link out of the tree: A/h -> out/h-out", || {
        io(fs::hard_link(j(&a, "h"), j(&out, "h-out")), "link")
    });
    m.step("write+close A/h's content through the outside link out/h-out", || {
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&out, "h-out")), "open")?;
        io(f.write_all(b"through the other name"), "write")
    });
    let mut tmp: Option<File> = None;
    m.step("O_TMPFILE in A, write 3 bytes (no name yet)", || {
        let mut f = io(
            fs::OpenOptions::new().write(true).custom_flags(libc::O_TMPFILE).mode(0o644).open(&a),
            "O_TMPFILE",
        )?;
        io(f.write_all(b"tmp"), "write")?;
        tmp = Some(f);
        Ok(())
    });
    m.step("... linkat it in as A/linked", || {
        let f = tmp.as_ref().ok_or("no tmpfile")?;
        let from = CString::new(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap();
        let to = cpath(&j(&a, "linked"));
        let rc = unsafe {
            libc::linkat(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::AT_SYMLINK_FOLLOW)
        };
        if rc < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("... then close it", || {
        drop(tmp.take());
        Ok(())
    });
    m.step("renameat2(RENAME_EXCHANGE) A/h2 <-> A/chm", || {
        let x = cpath(&j(&a, "h2"));
        let y = cpath(&j(&a, "chm"));
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, x.as_ptr(), libc::AT_FDCWD, y.as_ptr(), libc::RENAME_EXCHANGE)
        };
        if rc < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("create+write A/thr from a second thread of this process", || {
        let p = j(&a, "thr");
        thread::spawn(move || write_file(&p, b"t")).join().map_err(|_| "thread panicked".to_string())?
    });
    m.step("create+write A/child from a child process (/bin/sh -c 'echo x > A/child')", || {
        let st = io(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo x > '{}'", j(&a, "child").display()))
                .status(),
            "spawn",
        )?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("sh exited {st}"))
        }
    });
    let sh_append = |p: PathBuf| -> Result<(), String> {
        let st = io(
            Command::new("/bin/sh").arg("-c").arg(format!("echo y >> '{}'", p.display())).status(),
            "spawn",
        )?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("sh exited {st}"))
        }
    };
    m.step("A/mixed: this process creates+writes it, then a child process appends, nothing read between", || {
        write_file(&j(&a, "mixed"), b"x")?;
        sh_append(j(&a, "mixed"))
    });
    m.step("A/mixed: a child process appends, then this process appends, nothing read between", || {
        sh_append(j(&a, "mixed"))?;
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&a, "mixed")), "open")?;
        io(f.write_all(b"z"), "write")
    });
    let moved = base.join("tree-moved");
    m.step("rename the marked root: tree -> tree-moved (its parent is not marked)", || {
        io(fs::rename(&tree, &moved), "rename")
    });
    m.labels.move_dir("tree", &moved);
    m.labels.move_dir("A", &moved.join("A"));
    m.labels.move_dir("B", &moved.join("B"));
    m.step("rm -rf tree-moved", || io(fs::remove_dir_all(&moved), "remove_dir_all"));
    thread::sleep(Duration::from_millis(300));
    let late = read_events(&m.group);
    println!("late events, 300 ms later: {}", late.len());
    for ev in &late {
        println!("    {}", m.labels.fmt(ev));
    }
    0
}

fn child_fill(dir: &Path) -> i32 {
    let group = design_group();
    fan_mark(&group, fan::MARK_ADD, fan::DESIGN_MASK | fan::MODIFY, dir).unwrap();
    let mut labels = Labels::new(fsid_of(dir));
    labels.add_dir("F", dir);
    for n in FILL_FILES {
        labels.add_obj(n, &dir.join(n));
    }
    let mut m = Matrix { group, labels };
    for name in &FILL_FILES[..6] {
        let p = dir.join(name);
        m.step(&format!("open {name} O_RDONLY, read it, close (the root group answers the open)"), || {
            let mut f = io(File::open(&p), "open")?;
            let mut v = Vec::new();
            io(f.read_to_end(&mut v), "read")?;
            Ok(())
        });
    }
    let deny = dir.join("denyme");
    let mut note = String::new();
    m.step("open(F/denyme, O_CREAT|O_WRONLY) while the root group denies opens of that name", || {
        let r = fs::OpenOptions::new().write(true).create(true).open(&deny);
        let exists = deny.exists();
        match r {
            Ok(_) => Err(format!("the open succeeded; file exists: {exists}")),
            Err(e) => {
                note = format!("the open failed {}; the file exists afterwards: {exists}", io_err(&e));
                Ok(())
            }
        }
    });
    println!("    ({note})");

    // The same kinds of write, on a descriptor this process opened itself:
    // what the daemon's own writes look like to its own group.
    let own = dir.join("fill-own");
    let f = match fs::OpenOptions::new().read(true).write(true).open(&own) {
        Ok(f) => f,
        Err(e) => {
            println!("open fill-own: {}", io_err(&e));
            return 1;
        }
    };
    let fd = f.as_raw_fd();
    m.step("own descriptor on F/fill-own (O_RDWR): open", || Ok(()));
    m.step("own descriptor: pwrite 5 bytes", || {
        let n = unsafe { libc::pwrite(fd, b"HELLO".as_ptr().cast(), 5, 0) };
        if n < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: ftruncate(16384)", || {
        if unsafe { libc::ftruncate(fd, 16384) } < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: futimens", || {
        let ts = [
            libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
            libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
        ];
        if unsafe { libc::futimens(fd, ts.as_ptr()) } < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: fsetxattr user.konedrive.state", || {
        let key = CString::new("user.konedrive.state").unwrap();
        if unsafe { libc::fsetxattr(fd, key.as_ptr(), b"hydrated".as_ptr().cast(), 8, 0) } < 0 {
            Err(ename(last_errno()))
        } else {
            Ok(())
        }
    });
    m.step("own descriptor: fallocate(PUNCH_HOLE|KEEP_SIZE)", || {
        let r = unsafe {
            libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 0, 4096)
        };
        if r < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: close", || {
        drop(f);
        Ok(())
    });
    0
}

fn child_overflow(dir: &Path) -> i32 {
    let max: usize = read_sysctl("max_queued_events").parse().unwrap_or(16384);
    let group = design_group();
    fan_mark(&group, fan::MARK_ADD, fan::CREATE, dir).unwrap();
    let n = max + 100;
    let t0 = Instant::now();
    for i in 0..n {
        File::create(dir.join(format!("f{i}"))).unwrap();
    }
    println!(
        "  max_queued_events = {max}; created {n} files in the marked directory in {:?}, reading nothing meanwhile",
        t0.elapsed()
    );
    let events = read_events(&group);
    let creates = events.iter().filter(|e| e.mask & fan::CREATE != 0).count();
    let overflows: Vec<(usize, &Ev)> =
        events.iter().enumerate().filter(|(_, e)| e.mask & fan::Q_OVERFLOW != 0).collect();
    println!("  read {} events: {creates} FAN_CREATE, {} FAN_Q_OVERFLOW", events.len(), overflows.len());
    for (i, e) in overflows {
        println!(
            "  overflow at position {i} of {}: mask {}, event_len {}, metadata_len {}, fd {}, pid {}, {} info record(s)",
            events.len(),
            mask_str(e.mask),
            e.event_len,
            e.metadata_len,
            e.fd,
            e.pid,
            e.recs.len()
        );
    }
    File::create(dir.join("after")).unwrap();
    let after = drain(&group);
    println!(
        "  after draining, one more create: {} event(s) {}",
        after.len(),
        after.iter().map(|e| mask_str(e.mask)).collect::<Vec<_>>().join(", ")
    );
    println!(
        "  FAN_UNLIMITED_QUEUE unprivileged: {}",
        ok_or(fan_init(fan::DESIGN_INIT | fan::UNLIMITED_QUEUE, libc::O_RDONLY as u32))
    );
    0
}

fn child_marklimit(dir: &Path) -> i32 {
    let limit = read_sysctl("max_user_marks");
    println!("  max_user_marks as the child reads it: {limit}");
    let dirs: Vec<PathBuf> = (0..60).map(|i| dir.join(format!("d{i}"))).collect();
    for d in &dirs {
        fs::create_dir(d).unwrap();
    }
    let g1 = design_group();
    let mut ok1 = 0;
    for d in &dirs[..25] {
        match fan_mark(&g1, fan::MARK_ADD, fan::DESIGN_MASK, d) {
            Ok(()) => ok1 += 1,
            Err(e) => {
                println!("  group 1: mark {} refused {}", ok1 + 1, ename(e));
                break;
            }
        }
    }
    println!("  group 1: {ok1} marks placed");
    let g2 = design_group();
    let mut ok2 = 0;
    let mut failed_at = None;
    for d in &dirs[25..] {
        match fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK, d) {
            Ok(()) => ok2 += 1,
            Err(e) => {
                failed_at = Some((d.clone(), e));
                break;
            }
        }
    }
    match &failed_at {
        Some((_, e)) => println!(
            "  group 2: {ok2} marks placed, the next refused {} — {} marks in total for this uid",
            ename(*e),
            ok1 + ok2
        ),
        None => println!("  group 2: {ok2} marks placed, no refusal"),
    }
    let again_same = fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK | fan::MODIFY, &dirs[25]);
    println!("  adding bits to a directory group 2 already marks, at the limit: {}", ok_or(again_same));
    if let Some((d, _)) = failed_at {
        let removed = fan_mark(&g1, fan::MARK_REMOVE, fan::DESIGN_MASK, &dirs[0]);
        println!("  group 1 removes one mark: {}", ok_or(removed));
        println!("  group 2 retries the refused directory: {}", ok_or(fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK, &d)));
    }
    0
}
