use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};

use crate::harness::{Checks, FAN_OPEN_PERM, Mark, parse_mark};
use crate::now_running;

// ---------------------------------------------------------------------------
// kernel facts the helper's design rests on, measured directly
// ---------------------------------------------------------------------------
//
// These need no helper and no daemon: they are the two results
// `docs/kernel-behavior-7.2.md` §10 listed as having no committed programme at
// all, both of them load-bearing. They run against a fanotify group of this
// process's own, in a directory nobody else has marked, before the helper for
// this filesystem is started.

/// A fanotify group set up exactly as `konedrive_helper::marks::Marks` sets
/// one up, minus the unlimited-queue and unlimited-mark flags nothing here
/// needs.
fn own_group() -> Result<Fanotify, String> {
    Fanotify::init(
        InitFlags::FAN_CLASS_PRE_CONTENT | InitFlags::FAN_CLOEXEC | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDWR | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
    )
    .map_err(|e| format!("cannot create a fanotify group: {e}"))
}

fn next_own_event(
    group: &Fanotify,
    within: Duration,
) -> Option<nix::sys::fanotify::FanotifyEvent> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match group.read_events() {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    return Some(event);
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Opens a file on another thread, so this one can answer the permission event
/// that open raises. fanotify does not exempt the listening process, so a
/// single-threaded version of this deadlocks.
struct ThreadOpener {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ThreadOpener {
    fn start(path: PathBuf, write: bool) -> (Self, Arc<Mutex<Option<File>>>) {
        let done = Arc::new(AtomicBool::new(false));
        let held: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
        let flag = Arc::clone(&done);
        let slot = Arc::clone(&held);
        let handle = std::thread::spawn(move || {
            let opened = File::options().read(true).write(write).open(&path);
            if let Ok(file) = opened {
                *slot.lock().unwrap() = Some(file);
            }
            flag.store(true, Ordering::SeqCst);
        });
        (ThreadOpener { done, handle: Some(handle) }, held)
    }

    fn finished(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.done.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Waits for the opener only if it has already returned. A thread still
    /// suspended in `open()` is released when the group's descriptor closes
    /// (`fanotify(7)`), which happens when the caller drops the group — so
    /// joining unconditionally is how a failed measurement becomes a hung
    /// programme.
    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            if self.done.load(Ordering::SeqCst) {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for ThreadOpener {
    fn drop(&mut self) {
        // Detached deliberately: see `join`.
        self.handle.take();
    }
}

fn own_marks(group: &Fanotify) -> Vec<Mark> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&group.as_fd());
    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .unwrap_or_default()
        .lines()
        .filter_map(parse_mark)
        .collect()
}

/// `docs/kernel-behavior-7.2.md` §5.1, which had no committed programme: a
/// permission response is matched against the descriptor **number** the kernel
/// handed out, not against the open file description behind it.
///
/// Both halves matter to the helper. The first is why `take_fd` keeps the
/// exact descriptor through the whole "ask the daemon and wait" path instead
/// of a convenient duplicate. The second is the dangerous one: a closed
/// number is immediately reusable, so answering a remembered number after
/// closing it can answer somebody else's event.
fn response_matched_by_number(dir: &Path) -> Result<(), String> {
    // Created before the mark: a file created *inside* a marked directory
    // raises a permission event aimed at this process, and the thread that
    // would have to answer it is the one blocked in `open()` (kernel fact 7).
    let path = dir.join("numbered.bin");
    std::fs::write(&path, b"numbered").map_err(|e| e.to_string())?;

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;

    let (opener, _held) = ThreadOpener::start(path.clone(), false);
    let event = next_own_event(&group, Duration::from_secs(5))
        .ok_or("the open raised no permission event")?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&event.fd().ok_or("no descriptor on the event")?);
    std::mem::forget(event);

    // A duplicate has the same open file description and a different number.
    // SAFETY: `raw` is an open descriptor this process owns.
    let duplicate = unsafe { libc::dup(raw) };
    if duplicate < 0 {
        return Err(format!("cannot duplicate the event fd: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `duplicate` was just returned by `dup` and is owned here.
    let duplicate = unsafe { OwnedFd::from_raw_fd_checked(duplicate) };
    let by_duplicate = group
        .write_response(FanotifyResponse::new(duplicate.as_fd(), Response::FAN_ALLOW));
    match by_duplicate {
        Err(nix::errno::Errno::ENOENT) => {}
        Err(other) => {
            return Err(format!(
                "answering with a duplicate failed with {other}, not the ENOENT §5.1 records"
            ))
        }
        Ok(()) => {
            // The opener has been released; there is nothing left to measure
            // and the finding itself is the important part.
            let _ = opener.finished(Duration::from_secs(5));
            opener.join();
            return Err(
                "a duplicate of the event fd ANSWERED the event: the helper's rule that an event \
                 must be answered with its own descriptor no longer holds"
                    .into(),
            );
        }
    }
    if opener.finished(Duration::from_millis(300)) {
        return Err("the opener was released by a response that reported failure".into());
    }

    // Now the second half: close the number, then answer with it anyway.
    // SAFETY: closing a descriptor this process owns and will not use again
    // through this path.
    unsafe { libc::close(raw) };
    // SAFETY: `raw` is used only as the number the response names, which is
    // exactly what is under test; nothing reads or writes through it.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    let after_close = group.write_response(FanotifyResponse::new(borrowed, Response::FAN_ALLOW));
    let released = opener.finished(Duration::from_secs(5));
    // Closing the group is what releases an opener whose event nothing
    // answered, so it happens before the join and before anything returns.
    drop(group);
    let _ = opener.finished(Duration::from_secs(5));
    opener.join();
    drop(duplicate);
    match (after_close, released) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => {
            Err("the response naming a closed number succeeded but the opener stayed blocked"
                .into())
        }
        (Err(e), _) => Err(format!(
            "answering with the original number after closing it failed with {e}; §5.1 says it \
             succeeds, and a number that can be recycled while still answerable is what makes \
             `take_fd`'s ownership rule load-bearing"
        )),
    }
}

/// `docs/kernel-behavior-7.2.md` §2.1's decisive row, which had no committed
/// programme: an ignore mark without `FAN_MARK_IGNORED_SURV_MODIFY` is
/// silently refused whenever **anybody** holds the inode open for writing —
/// no event fd is involved. That is what proves the refusal comes from
/// `inode_is_open_for_write()` rather than from anything fanotify-specific,
/// and therefore that `SURV_MODIFY` is not optional for a helper whose event
/// fds are always `O_RDWR`.
fn ignore_without_surv_modify(dir: &Path) -> Result<(), String> {
    // Created before the mark, for the reason in `response_matched_by_number`.
    let name = "surv-modify.bin";
    let path = dir.join(name);
    std::fs::write(&path, b"x").map_err(|e| e.to_string())?;
    let ino = std::fs::metadata(&path).map_err(|e| e.to_string())?.ino();

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;
    let present = |group: &Fanotify| {
        own_marks(group).iter().any(|m| m.ino == ino && m.ignored_mask & FAN_OPEN_PERM != 0)
    };
    // Always by (dirfd, name): resolving a name raises no event, while
    // opening the file would raise one aimed at this very process. Path
    // resolution is therefore held constant and the only variable is what
    // descriptor happens to be open.
    let plain =
        MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_IGNORE | MarkFlags::FAN_MARK_EVICTABLE;
    let add = |flags: MarkFlags| {
        group.mark(flags, MaskFlags::FAN_OPEN_PERM, handle.as_fd(), Some(Path::new(name)))
    };
    let remove = || {
        let _ = group.mark(
            MarkFlags::FAN_MARK_REMOVE | MarkFlags::FAN_MARK_IGNORE,
            MaskFlags::FAN_OPEN_PERM,
            handle.as_fd(),
            Some(Path::new(name)),
        );
    };

    // The control: with nothing open on the inode, the plain ignore mark
    // lands. Without this row the row below would only show that something
    // was wrong, not what.
    add(plain).map_err(|e| format!("the plain ignore mark was refused outright: {e}"))?;
    if !present(&group) {
        return Err("a plain ignore mark on an idle file did not appear in fdinfo either".into());
    }
    remove();
    if present(&group) {
        return Err("the ignore mark could not be removed again".into());
    }

    // The decisive row: an ordinary `O_RDWR` descriptor, opened by another
    // thread of this process and answered from here, with no event fd
    // anywhere near the mark.
    let (opener, held) = ThreadOpener::start(path.clone(), true);
    let event = next_own_event(&group, Duration::from_secs(5))
        .ok_or("the writable open raised no permission event")?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&event.fd().ok_or("no descriptor on the event")?);
    std::mem::forget(event);
    // SAFETY: `raw` is the descriptor the kernel handed out for this event;
    // it is answered and closed exactly once here.
    let owned = unsafe { OwnedFd::from_raw_fd_checked(raw) };
    group
        .write_response(FanotifyResponse::new(owned.as_fd(), Response::FAN_ALLOW))
        .map_err(|e| format!("cannot allow the writable open: {e}"))?;
    drop(owned);
    if !opener.finished(Duration::from_secs(5)) {
        return Err("the writable opener never returned".into());
    }
    let writable = held.lock().unwrap().is_some();
    if !writable {
        return Err("the writable open failed, so nothing holds the inode open for write".into());
    }

    let refused_silently = add(plain).is_ok() && !present(&group);
    let with_surv = add(plain | MarkFlags::FAN_MARK_IGNORED_SURV_MODIFY).is_ok() && present(&group);
    remove();
    *held.lock().unwrap() = None;
    opener.join();

    if !refused_silently {
        return Err(
            "an ignore mark without FAN_MARK_IGNORED_SURV_MODIFY was NOT silently refused while \
             an ordinary O_RDWR descriptor was open: §2.1 no longer holds, and the flag's \
             justification has to be rewritten"
                .into(),
        );
    }
    if !with_surv {
        return Err(
            "FAN_MARK_IGNORED_SURV_MODIFY did not make the mark land either: the helper's ignore \
             marks do not exist at all"
                .into(),
        );
    }
    Ok(())
}

/// A small, safe-ish wrapper around `OwnedFd::from_raw_fd`, kept in one place
/// so the unsafety is spelled out once.
trait FromRawFdChecked {
    /// # Safety
    /// `raw` must be an open descriptor this process owns and which nothing
    /// else will close.
    unsafe fn from_raw_fd_checked(raw: libc::c_int) -> OwnedFd;
}

impl FromRawFdChecked for OwnedFd {
    unsafe fn from_raw_fd_checked(raw: libc::c_int) -> OwnedFd {
        use std::os::fd::FromRawFd;
        unsafe { OwnedFd::from_raw_fd(raw) }
    }
}

/// Whether a **read-only** opener suspended in a fanotify permission wait
/// already counts against a write lease.
///
/// Dehydration (`docs/design/hydration.md` §8) empties a file under a write
/// lease, on the promise that the lease
/// is refused while anybody has the file open. An opener suspended in a
/// permission wait has no descriptor yet, and one the helper lets through
/// still has to get past `break_lease()` afterwards. If the kernel counted a
/// read-only open (`i_readcount`) only once the open had completed, a
/// dehydration could take its lease in that gap — after the helper let the
/// opener through onto a `hydrated` file, before the opener's own
/// `break_lease()` — and the opener, woken when the lease went, would read the
/// punched file. The helper's own event descriptor refuses the lease while it
/// is open (it is `O_RDWR`), but it is closed the moment the answer is
/// written, before the opener runs again.
///
/// Measured with this process's own group and nobody reading its events, so
/// that no event descriptor exists: while the opener is suspended, the only
/// thing that can refuse the lease is the opener itself.
fn suspended_reader_refuses_a_lease(dir: &Path) -> Result<(), String> {
    use konedrive_fs::lease::WriteLease;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let path = dir.join("leased.bin");
    std::fs::write(&path, b"leased").map_err(|e| e.to_string())?;
    // Opened before the mark: this process's own opens are events too.
    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    match WriteLease::take(&file).map_err(|e| e.to_string())? {
        Some(lease) => drop(lease),
        None => return Err("the control failed: a lease was refused with nothing else open".into()),
    }

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;
    let (opener, _held) = ThreadOpener::start(path.clone(), false);
    // Queued, and deliberately not read: reading would create the event's
    // own descriptor, which refuses the lease by itself.
    let mut fds = [PollFd::new(group.as_fd(), PollFlags::POLLIN)];
    let queued = matches!(poll(&mut fds, PollTimeout::from(5000u16)), Ok(n) if n > 0);
    let refused = if queued {
        match WriteLease::take(&file).map_err(|e| e.to_string()) {
            Ok(Some(lease)) => {
                drop(lease);
                Some(false)
            }
            Ok(None) => Some(true),
            Err(e) => {
                drop(group);
                opener.join();
                return Err(format!("the lease failed outright: {e}"));
            }
        }
    } else {
        None
    };
    // Closing the group releases the opener.
    drop(group);
    let released = opener.finished(Duration::from_secs(5));
    opener.join();
    match (refused, released) {
        (None, _) => Err("the read-only open raised no permission event".into()),
        (_, false) => Err("the opener was not released when the group closed".into()),
        (Some(true), true) => Ok(()),
        (Some(false), true) => Err(
            "a write lease was GRANTED while a read-only opener was suspended in a permission \
             wait: an opener the helper lets through can be overtaken by a dehydration's lease \
             and read the file it punches"
                .into(),
        ),
    }
}

pub(crate) fn kernel_facts(fs: &'static str, checks: &mut Checks) {
    let dir = PathBuf::from(format!("/mnt/{fs}/facts"));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        checks.record(fs, "the kernel-fact checks could run", Err(e.to_string()));
        return;
    }
    now_running("kernel fact: a response is matched by descriptor number");
    checks.record(
        fs,
        "a permission response is matched by descriptor number, not by open file description",
        response_matched_by_number(&dir),
    );
    now_running("kernel fact: an ignore mark without SURV_MODIFY is refused");
    checks.record(
        fs,
        "an ignore mark without SURV_MODIFY is silently refused while the inode is open for write",
        ignore_without_surv_modify(&dir),
    );
    now_running("kernel fact: a suspended read-only opener refuses a write lease");
    checks.record(
        fs,
        "a read-only opener suspended in a permission wait already refuses a write lease",
        suspended_reader_refuses_a_lease(&dir),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
