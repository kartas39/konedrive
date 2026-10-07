use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::fan;
use crate::sys::{Handle, ename, last_errno, name_to_handle, show_fsid};

// ---------------------------------------------------------------------------
// Reading and printing events of a FID-reporting group
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Rec {
    pub(crate) itype: u8,
    pub(crate) len: u16,
    pub(crate) fsid: [i32; 2],
    pub(crate) handle: Option<Handle>,
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Ev {
    pub(crate) event_len: u32,
    pub(crate) metadata_len: u16,
    pub(crate) mask: u64,
    pub(crate) fd: i32,
    pub(crate) pid: i32,
    pub(crate) recs: Vec<Rec>,
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
pub(crate) fn read_events(group: &OwnedFd) -> Vec<Ev> {
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
pub(crate) fn drain(group: &OwnedFd) -> Vec<Ev> {
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

pub(crate) fn mask_str(mask: u64) -> String {
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

pub(crate) fn info_name(itype: u8) -> &'static str {
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
pub(crate) struct Labels {
    known: HashMap<Handle, String>,
    dirs: HashMap<String, PathBuf>,
    fsid: [i32; 2],
    next: usize,
    me: i32,
}

impl Labels {
    pub(crate) fn new(fsid: [i32; 2]) -> Self {
        Labels {
            known: HashMap::new(),
            dirs: HashMap::new(),
            fsid,
            next: 0,
            me: std::process::id() as i32,
        }
    }

    pub(crate) fn add_dir(&mut self, label: &str, path: &Path) {
        let (h, _) = name_to_handle(path, 0)
            .unwrap_or_else(|e| panic!("name_to_handle_at {path:?}: {}", ename(e)));
        self.known.insert(h, label.to_string());
        self.dirs.insert(label.to_string(), path.to_path_buf());
    }

    pub(crate) fn move_dir(&mut self, label: &str, path: &Path) {
        self.dirs.insert(label.to_string(), path.to_path_buf());
    }

    pub(crate) fn add_obj(&mut self, label: &str, path: &Path) {
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

    pub(crate) fn fmt(&mut self, ev: &Ev) -> String {
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
