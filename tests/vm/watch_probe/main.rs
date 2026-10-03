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

mod child;
mod events;
mod root;
mod sys;

use std::thread;
use std::time::Duration;

use crate::child::child_main;
use crate::root::root_main;

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
