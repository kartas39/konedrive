//! The notification group itself: `fanotify_init`, `fanotify_mark` and the
//! events it reports, all without privilege (`docs/design/writes.md` §3 as amended
//! by §17; `docs/kernel-behavior-7.2.md` §14).
//!
//! The group reports file handles: every event on an entry carries its
//! directory and name (`DFID_NAME`) and the object's own handle (`FID`); a
//! rename carries the old and the new side (`OLD_DFID_NAME`,
//! `NEW_DFID_NAME`), but only the sides whose directory this group marks.
//! An event on a directory itself carries only `DFID_NAME(<that directory>,
//! ".")`. An overflow is one bare event at the tail of the queue.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;

use konedrive_fs::handle::{FileHandle, MAX_HANDLE_BYTES};

// `include/uapi/linux/fanotify.h`, spelled out as the probe does: which of
// them a given libc release exports varies.
const CLOEXEC: u32 = 0x1;
const NONBLOCK: u32 = 0x2;
const CLASS_NOTIF: u32 = 0x0;
const REPORT_FID: u32 = 0x200;
const REPORT_DIR_FID: u32 = 0x400;
const REPORT_NAME: u32 = 0x800;
const REPORT_TARGET_FID: u32 = 0x1000;
/// `FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME_TARGET | FAN_NONBLOCK |
/// FAN_CLOEXEC`: what an unprivileged process may ask for that names both
/// the directory entry and the object (§14.1).
const INIT: u32 = CLASS_NOTIF | REPORT_DIR_FID | REPORT_NAME | REPORT_FID | REPORT_TARGET_FID | NONBLOCK | CLOEXEC;

const MARK_ADD: u32 = 0x1;
const MARK_ONLYDIR: u32 = 0x8;

pub const ATTRIB: u64 = 0x4;
pub const CLOSE_WRITE: u64 = 0x8;
pub const CREATE: u64 = 0x100;
pub const DELETE: u64 = 0x200;
pub const DELETE_SELF: u64 = 0x400;
pub const MOVE_SELF: u64 = 0x800;
pub const Q_OVERFLOW: u64 = 0x4000;
pub const EVENT_ON_CHILD: u64 = 0x0800_0000;
pub const RENAME: u64 = 0x1000_0000;
pub const ONDIR: u64 = 0x4000_0000;

/// Every directory of the folder. No `FAN_MOVED_FROM`/`FAN_MOVED_TO`:
/// `FAN_RENAME` says all they do, and they would triple what a rename costs
/// in the queue (§3.1). No `FAN_MODIFY`: a change is examined once it is
/// closed (`FAN_CLOSE_WRITE`) or its attributes move (`FAN_ATTRIB`).
pub const DIR_MASK: u64 = CREATE | DELETE | RENAME | CLOSE_WRITE | ATTRIB | ONDIR | EVENT_ON_CHILD;
/// The root also reports being moved or deleted (§3.3).
pub const ROOT_MASK: u64 = DIR_MASK | DELETE_SELF | MOVE_SELF;

const INFO_FID: u8 = 1;
const INFO_DFID_NAME: u8 = 2;
const INFO_OLD_DFID_NAME: u8 = 10;
const INFO_NEW_DFID_NAME: u8 = 12;

/// A filesystem's id as `statfs(2)` gives it, and as every record carries
/// it. Each Btrfs subvolume has its own.
pub type Fsid = [i32; 2];

/// An object as the kernel names it: its filesystem and its file handle.
/// Byte-equal to what `name_to_handle_at` gives (§14.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fid {
    pub fsid: Fsid,
    pub handle: FileHandle,
}

impl Fid {
    /// The object `file` is open on.
    pub fn of(file: &File) -> io::Result<Self> {
        Ok(Self { fsid: fsid_of(file)?, handle: FileHandle::of(file)? })
    }
}

/// A directory and a name in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub dir: Fid,
    pub name: OsString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub mask: u64,
    /// The pid of the process that caused it when that is the listener's
    /// own, and 0 otherwise (an unprivileged listener sees no other pid).
    pub pid: i32,
    /// `DFID_NAME`.
    pub at: Option<Named>,
    /// `OLD_DFID_NAME`: a rename's old side, if this group marks it.
    pub old: Option<Named>,
    /// `NEW_DFID_NAME`: a rename's new side, if this group marks it.
    pub new: Option<Named>,
    /// `FID`: the object the event is about.
    pub object: Option<Fid>,
}

impl Event {
    pub fn has(&self, bits: u64) -> bool {
        self.mask & bits != 0
    }
}

/// The filesystem id of what `file` is open on.
pub fn fsid_of(file: &File) -> io::Result<Fsid> {
    // SAFETY: `st` is a zeroed `statfs` that `fstatfs` fills.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fsid_t` is two ints; its field is private in the libc crate.
    Ok(unsafe { *(&st.f_fsid as *const libc::fsid_t as *const Fsid) })
}

/// One notification group, for directories of one filesystem id: a group
/// that marks one Btrfs subvolume refuses a mark on another with `EXDEV`
/// (§3.6).
pub struct Group {
    fd: OwnedFd,
    pub fsid: Fsid,
}

impl Group {
    /// `EMFILE` when the uid has its 128 groups already (§14.2).
    pub fn new(fsid: Fsid) -> io::Result<Self> {
        // SAFETY: plain syscall; the descriptor it returns is owned here.
        let fd = unsafe { libc::fanotify_init(INIT, (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE) as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned and nothing else owns it.
        Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) }, fsid })
    }

    /// An inode mark on the directory `dir` is open on: by descriptor, so no
    /// rename can redirect it. `ENOSPC` when the uid's mark budget is spent
    /// (§14.2); adding bits to a directory already marked costs nothing.
    pub fn mark(&self, dir: &File, mask: u64) -> io::Result<()> {
        // SAFETY: a NULL path marks the object `dir` is open on.
        let rc = unsafe { libc::fanotify_mark(self.fd.as_raw_fd(), MARK_ADD | MARK_ONLYDIR, mask, dir.as_raw_fd(), std::ptr::null()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Reads one buffer's worth of queued events into `out`. `false` when
    /// the queue was empty.
    pub fn read(&self, buf: &mut [u8], out: &mut Vec<Event>) -> io::Result<bool> {
        // SAFETY: `buf` is writable for its whole length.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EAGAIN) => Ok(false),
                Some(libc::EINTR) => Ok(true),
                _ => Err(e),
            };
        }
        parse(&buf[..n as usize], out);
        Ok(n > 0)
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

/// `struct fanotify_event_metadata` is 24 bytes.
const METADATA: usize = 24;

/// The events in `buf`, as `read(2)` returned them. A malformed tail is
/// dropped: the kernel never writes one.
pub fn parse(buf: &[u8], out: &mut Vec<Event>) {
    let mut off = 0;
    while off + METADATA <= buf.len() {
        let len = u32_at(buf, off) as usize;
        if len < METADATA || off + len > buf.len() {
            break;
        }
        let end = off + len;
        let metadata_len = u16_at(buf, off + 6) as usize;
        let mask = u64::from_ne_bytes(buf[off + 8..off + 16].try_into().unwrap());
        let fd = i32_at(buf, off + 16);
        let pid = i32_at(buf, off + 20);
        if fd >= 0 {
            // A FID-reporting group opens nothing; close it if it ever did.
            // SAFETY: the kernel handed this descriptor to us.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let mut event = Event { mask, pid, at: None, old: None, new: None, object: None };
        let mut r = off + metadata_len.max(METADATA);
        while r + 4 <= end {
            let kind = buf[r];
            let rlen = u16_at(buf, r + 2) as usize;
            if rlen < 4 || r + rlen > end {
                break;
            }
            if let Some((fid, name)) = fid_record(&buf[r..r + rlen]) {
                match kind {
                    INFO_FID => event.object = Some(fid),
                    INFO_DFID_NAME => event.at = name.map(|name| Named { dir: fid, name }),
                    INFO_OLD_DFID_NAME => event.old = name.map(|name| Named { dir: fid, name }),
                    INFO_NEW_DFID_NAME => event.new = name.map(|name| Named { dir: fid, name }),
                    _ => {}
                }
            }
            r += rlen;
        }
        out.push(event);
        off = end;
    }
}

/// `struct fanotify_event_info_fid`: header, fsid, `struct file_handle`,
/// then for the `*_DFID_NAME` kinds a NUL-terminated name.
fn fid_record(rec: &[u8]) -> Option<(Fid, Option<OsString>)> {
    if rec.len() < 20 {
        return None;
    }
    let fsid = [i32_at(rec, 4), i32_at(rec, 8)];
    let bytes = u32_at(rec, 12) as usize;
    let kind = i32_at(rec, 16);
    if bytes > MAX_HANDLE_BYTES || 20 + bytes > rec.len() {
        return None;
    }
    let handle = FileHandle { kind, bytes: rec[20..20 + bytes].to_vec() };
    let name = match rec[0] {
        INFO_DFID_NAME | INFO_OLD_DFID_NAME | INFO_NEW_DFID_NAME => {
            let tail = &rec[20 + bytes..];
            let nul = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
            Some(OsString::from_vec(tail[..nul].to_vec()))
        }
        _ => None,
    };
    Some((Fid { fsid, handle }, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, fsid: Fsid, handle: &FileHandle, name: Option<&str>) -> Vec<u8> {
        let mut rec = vec![kind, 0, 0, 0];
        rec.extend_from_slice(&fsid[0].to_ne_bytes());
        rec.extend_from_slice(&fsid[1].to_ne_bytes());
        rec.extend_from_slice(&(handle.bytes.len() as u32).to_ne_bytes());
        rec.extend_from_slice(&handle.kind.to_ne_bytes());
        rec.extend_from_slice(&handle.bytes);
        if let Some(name) = name {
            rec.extend_from_slice(name.as_bytes());
            rec.push(0);
        }
        while rec.len() % 4 != 0 {
            rec.push(0);
        }
        let len = rec.len() as u16;
        rec[2..4].copy_from_slice(&len.to_ne_bytes());
        rec
    }

    fn event(mask: u64, pid: i32, records: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = records.concat();
        let mut ev = Vec::new();
        ev.extend_from_slice(&((METADATA + body.len()) as u32).to_ne_bytes());
        ev.extend_from_slice(&[3, 0]);
        ev.extend_from_slice(&(METADATA as u16).to_ne_bytes());
        ev.extend_from_slice(&mask.to_ne_bytes());
        ev.extend_from_slice(&(-1i32).to_ne_bytes());
        ev.extend_from_slice(&pid.to_ne_bytes());
        ev.extend_from_slice(&body);
        ev
    }

    #[test]
    fn a_rename_and_an_overflow_are_read_as_the_kernel_writes_them() {
        let dir = FileHandle { kind: 1, bytes: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        let obj = FileHandle { kind: 1, bytes: vec![9; 8] };
        let fsid = [7, -3];
        let mut buf = event(
            RENAME,
            42,
            &[record(INFO_OLD_DFID_NAME, fsid, &dir, Some("a")), record(INFO_NEW_DFID_NAME, fsid, &dir, Some("b")), record(INFO_FID, fsid, &obj, None)],
        );
        buf.extend(event(Q_OVERFLOW, 0, &[]));
        buf.extend(event(RENAME, 0, &[record(INFO_NEW_DFID_NAME, fsid, &dir, Some("in")), record(INFO_FID, fsid, &obj, None)]));
        let mut out = Vec::new();
        parse(&buf, &mut out);
        assert_eq!(out.len(), 3);
        let d = Fid { fsid, handle: dir };
        assert_eq!(out[0].old, Some(Named { dir: d.clone(), name: "a".into() }));
        assert_eq!(out[0].new, Some(Named { dir: d.clone(), name: "b".into() }));
        assert_eq!(out[0].object, Some(Fid { fsid, handle: obj.clone() }));
        assert_eq!(out[0].pid, 42);
        assert!(out[1].has(Q_OVERFLOW) && out[1].at.is_none() && out[1].object.is_none());
        assert_eq!((out[2].old.as_ref(), out[2].new.as_ref().map(|n| n.name.clone())), (None, Some("in".into())), "one side only");
    }
}
