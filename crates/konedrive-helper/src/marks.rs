//! The fanotify permission group: marks on directories, ignore marks on files.
//!
//! Every fanotify call in this file follows the two rules the proof of
//! concept in `tests/vm/poc_marks.rs` established (`docs/kernel-behavior-7.2.md`,
//! §7): fanotify does not exempt the process holding the group, so
//!
//! 1. files are marked by `(dirfd, name)` or by an fd we did not open
//!    ourselves (an event fd, or one handed to us over the daemon socket) —
//!    never by `File::open`ing the file, which would raise an event aimed at
//!    us and deadlock a single-threaded caller; an `O_PATH` fd is rejected by
//!    `fanotify_mark` with `EBADF`, so it is not a way around this either;
//! 2. directories are always safe to `File::open`: without `FAN_ONDIR` a
//!    directory open raises no event.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use konedrive_fs::MAX_DEPTH;
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};

/// The kernel's accepted `FAN_DENY` errno set and the clamp onto it now live
/// in `konedrive-proto` (Ruling H51), next to the `HydrateDone` message whose
/// `errno` field they constrain, so that the daemon produces only deliverable
/// values and this helper is not the only thing standing between an
/// undeliverable one and an opener suspended forever. Re-exported here so
/// every existing `marks::clamp_deny_errno` call site keeps working.
pub use konedrive_proto::{clamp_deny_errno, ACCEPTED_DENY_ERRNOS};

/// `FAN_MARK_REMOVE` on an object that carries no mark returns `ENOENT`, and
/// that is a **normal** outcome here rather than a failure: ignore marks are
/// added `FAN_MARK_EVICTABLE`, so the kernel is entitled to drop one at any
/// moment under memory pressure (`docs/kernel-behavior-7.2.md` §2), and the
/// daemon clears an ignore mark before every dehydration whether or not the
/// mark survived that long. Reporting it as an error is what made a routine
/// `ClearIgnore` tear down the daemon connection.
fn tolerate_missing_mark(result: nix::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub struct Marks {
    group: Fanotify,
}

impl Marks {
    pub fn new() -> io::Result<Self> {
        let group = Fanotify::init(
            // FAN_NONBLOCK is load-bearing: without it `read_events()` blocks
            // forever instead of returning EAGAIN once the queue is drained
            // (kernel fact 3; confirmed by the proof of concept's own
            // `group()` helper). main.rs's event_loop needs that: it waits
            // for readability with `poll()` and then drains with
            // `read_events()` until EAGAIN, which only works if EAGAIN is
            // ever actually returned.
            InitFlags::FAN_CLASS_PRE_CONTENT
                | InitFlags::FAN_CLOEXEC
                | InitFlags::FAN_UNLIMITED_QUEUE
                | InitFlags::FAN_UNLIMITED_MARKS
                | InitFlags::FAN_NONBLOCK,
            // `O_NONBLOCK` on the event descriptors is load-bearing too
            // (Ruling H140, the final review's I2). The kernel opens each
            // event's descriptor inside our `read()`, and opening a file that
            // somebody holds a write lease on waits for the lease to break:
            // without it, one leased file in a marked directory stopped this
            // helper's whole event loop — every intercepted open on the
            // machine waited behind it — for as long as the lease was held,
            // up to `lease-break-time` (45 s). Our own dehydration holds such
            // a lease across its punch, and any local user can take one on a
            // file of their own. With it, that one open cannot be handed over
            // and the kernel answers it itself: `FAN_DENY`, which the opener
            // sees as `EPERM` — never left suspended, never allowed. Measured
            // in the VM suite (`leased_file_does_not_stall_others`); see
            // `docs/kernel-behavior-7.2.md` §12.4. The flag changes nothing
            // else about a descriptor on a regular file: the daemon's
            // `pwrite`s and `fsync`s through it are unaffected.
            EventFFlags::O_RDWR
                | EventFFlags::O_LARGEFILE
                | EventFFlags::O_CLOEXEC
                | EventFFlags::O_NONBLOCK,
        )?;
        Ok(Self { group })
    }

    pub fn group(&self) -> &Fanotify {
        &self.group
    }

    /// Cover every file inside this directory. No FAN_ONDIR: opening the
    /// directory itself is never intercepted, so listing never waits for us.
    pub fn mark_dir(&self, dir: BorrowedFd<'_>) -> io::Result<()> {
        self.group.mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            dir,
            None::<&Path>,
        )?;
        Ok(())
    }

    pub fn unmark_dir(&self, dir: BorrowedFd<'_>) -> io::Result<()> {
        tolerate_missing_mark(self.group.mark(
            MarkFlags::FAN_MARK_REMOVE,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            dir,
            None::<&Path>,
        ))
    }

    /// Keeps a file covered after it left the sync root (invariant M4). `file`
    /// must be an fd we did not open ourselves — see the module doc.
    pub fn mark_file(&self, file: BorrowedFd<'_>) -> io::Result<()> {
        self.group.mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM,
            file,
            None::<&Path>,
        )?;
        Ok(())
    }

    /// Stops asking about a file whose content is already there (invariant
    /// M3: only ever called on a file just read `hydrated`, and taken off
    /// again unless it still reads `hydrated` once the mark is in place —
    /// `main.rs`, `mark_while_hydrated`, Rulings H5 and H139).
    ///
    /// # `FAN_MARK_IGNORED_SURV_MODIFY` is what makes this work at all
    ///
    /// Without that flag this call silently does nothing whenever anybody
    /// holds the file open for writing — it returns **0 and creates no mark**.
    /// `fanotify_add_inode_mark()` contains
    ///
    /// ```text
    /// if ((flags & FANOTIFY_MARK_IGNORE_BITS) &&
    ///     !(flags & FAN_MARK_IGNORED_SURV_MODIFY) &&
    ///     inode_is_open_for_write(inode))
    ///         return 0;
    /// ```
    ///
    /// and the helper is *always* in that situation: the event fd the kernel
    /// hands out for a permission event is `O_RDWR` (see `Marks::new`), and
    /// the daemon holds an `SCM_RIGHTS` copy of that same open file
    /// description until it has finished filling the file. Measured on Btrfs,
    /// ext4 and XFS (`docs/kernel-behavior-7.2.md` §2.1): with a writable
    /// descriptor open the mark never appears in `/proc/self/fdinfo/<group>`
    /// and the next open still raises an event; with `SURV_MODIFY` it appears
    /// and suppresses, whoever holds the inode open and whether the mark is
    /// placed before or after the response is written.
    ///
    /// The flag has a second effect that is wanted in its own right: an
    /// ignored mask *without* it is cleared by the kernel on every
    /// modification, so every save of a hydrated file would send its next
    /// open back to the helper for no reason.
    ///
    /// # What this flag took away: `ClearIgnore` is now safety-critical
    ///
    /// **Ruling H35. Never punch a hole in a file whose `ClearIgnore` did not
    /// succeed.**
    ///
    /// Without `SURV_MODIFY` the kernel cleared the ignored mask on *any*
    /// modification, so a dehydration that failed to clear the mark repaired
    /// itself the moment it punched: the next open was intercepted again and
    /// the file re-hydrated. That accident was doing real work, and this flag
    /// removes it. With the mask surviving modification, a dehydration that
    /// proceeds past a failed `ClearIgnore` leaves a file that is empty *and*
    /// permanently invisible to the helper — every later open is suppressed
    /// and the application reads **zeros, silently, for as long as the mark
    /// lives**. Nothing detects it and nothing repairs it.
    ///
    /// So the ordering in spec §8 — clear the ignore mark, take the write
    /// lease, then punch — is no longer an optimisation that saves a round
    /// trip. It is the thing standing between a dehydration and data loss,
    /// and a `ClearIgnore` that returns a non-zero errno must abort the
    /// dehydration, not be logged and stepped over. (`ENOENT` is *not* such a
    /// failure: an evictable mark is designed to vanish, and
    /// `tolerate_missing_mark` already reports that as success — "there is no
    /// mark" is exactly the state the caller wanted.)
    ///
    /// The second-party version of the same risk: an external tool that
    /// re-sparsified a managed file behind our back would leave the mask in
    /// place, and the file would read as zeros. That one we cannot prevent;
    /// the first-party one we can, by never punching on a failed clear.
    ///
    /// `FAN_MARK_EVICTABLE` is kept: measured, a `SURV_MODIFY` ignore mark is
    /// still dropped by `drop_caches` along with its inode, so the steady
    /// state is still free (§8) and callers still must not treat "an ignore
    /// mark exists" as durable.
    pub fn ignore_file(&self, file: BorrowedFd<'_>) -> io::Result<()> {
        self.group.mark(
            MarkFlags::FAN_MARK_ADD
                | MarkFlags::FAN_MARK_IGNORE
                | MarkFlags::FAN_MARK_IGNORED_SURV_MODIFY
                | MarkFlags::FAN_MARK_EVICTABLE,
            MaskFlags::FAN_OPEN_PERM,
            file,
            None::<&Path>,
        )?;
        Ok(())
    }

    /// Removed before dehydration (invariant M3), so the next open is
    /// intercepted again. Measured: a plain `FAN_MARK_REMOVE | FAN_MARK_IGNORE`
    /// clears an ignored mask that was added with `SURV_MODIFY`, and the very
    /// next open of the file raises an event again.
    ///
    /// **Ruling H35: the caller must not punch if this fails.** Since the
    /// ignore mask now survives modification, a file dehydrated while still
    /// ignored reads as zeros forever, with nothing to notice it — see
    /// [`ignore_file`](Self::ignore_file). `Ok(())` here means "the file
    /// carries no ignore mark", which includes the `ENOENT` case where the
    /// kernel had already evicted one; anything else must abort the
    /// dehydration.
    pub fn clear_ignore(&self, file: BorrowedFd<'_>) -> io::Result<()> {
        tolerate_missing_mark(self.group.mark(
            MarkFlags::FAN_MARK_REMOVE | MarkFlags::FAN_MARK_IGNORE,
            MaskFlags::FAN_OPEN_PERM,
            file,
            None::<&Path>,
        ))
    }

    /// [`clear_ignore`](Self::clear_ignore) for the entry `name` in `dir`,
    /// which is how [`walk_and_unmark`] reaches files: by `(dirfd, name)`,
    /// never by opening them (rule 1 of the module doc), and with
    /// `FAN_MARK_DONT_FOLLOW`, so a symlink is the object looked at rather
    /// than a way to reach something outside the tree.
    ///
    /// The name may have been swapped for something else since it was
    /// listed. That is harmless here in a way it would not be for an *add*:
    /// taking an ignore mark off any inode only means its next open is
    /// intercepted and decided afresh from its state, so the worst a race can
    /// buy is one extra permission event. `ENOENT` — no mark, an evicted one,
    /// or a name that is gone — is success, as for `clear_ignore`.
    pub fn clear_ignore_at(&self, dir: BorrowedFd<'_>, name: &CStr) -> io::Result<()> {
        tolerate_missing_mark(self.group.mark(
            MarkFlags::FAN_MARK_REMOVE
                | MarkFlags::FAN_MARK_IGNORE
                | MarkFlags::FAN_MARK_DONT_FOLLOW,
            MaskFlags::FAN_OPEN_PERM,
            dir,
            Some(name),
        ))
    }

    pub fn allow(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.group.write_response(FanotifyResponse::new(fd, Response::FAN_ALLOW))?;
        Ok(())
    }

    /// Deny with a real errno: `FAN_DENY` with the errno in the top byte.
    ///
    /// The errno is clamped to what the kernel accepts (M2) before it is
    /// used, and the byte is masked so that a value the clamp somehow let
    /// through cannot corrupt the response word. If even the clamped response
    /// is refused, a plain `FAN_DENY` is written instead — measured to rescue
    /// exactly this case — because an opener that is never answered stays
    /// blocked until the helper exits.
    pub fn deny(&self, fd: BorrowedFd<'_>, errno: i32) -> io::Result<()> {
        let errno = clamp_deny_errno(errno);
        let bits = Response::FAN_DENY.bits() | ((errno as u32 & 0xff) << 24);
        let response = Response::from_bits_retain(bits);
        match self.group.write_response(FanotifyResponse::new(fd, response)) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(
                    "the kernel refused FAN_DENY with errno {errno} ({e}); \
                     falling back to a plain FAN_DENY so the opener is answered"
                );
                self.group.write_response(FanotifyResponse::new(fd, Response::FAN_DENY))?;
                Ok(())
            }
        }
    }
}

/// What a startup, registration or unregistration walk managed to cover.
///
/// Ruling H19: a subdirectory the helper cannot open must never abort the
/// walk. Partial marking is strictly more coverage than none; the defect
/// worth fixing is the silence, so every failure is carried back here to be
/// logged by name and the root flagged degraded.
#[derive(Debug, Default)]
pub struct WalkReport {
    /// How many directories the walk actually changed — marked by
    /// [`walk_and_mark`], unmarked by [`walk_and_unmark`].
    pub marked: usize,
    pub failures: Vec<String>,
}

impl WalkReport {
    pub fn degraded(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Marks every directory under `root`, without ever opening a file, and
/// takes the ignore mark off every regular file it meets (Ruling H138).
/// Directory opens are exempt from interception (kernel fact 2's converse),
/// so this never risks deadlocking against our own marks; files are reached
/// by `(dirfd, name)` only (rule 1 of the module doc).
///
/// # Clearing is defence in depth, not a clean slate
///
/// Every way interception of a tree begins — a registration, and the
/// startup walk after the helper restarts — runs through here, and a stale
/// ignore mark is dangerous only once interception begins: in a tree nobody
/// intercepts, a file reads what it holds whatever marks it carries, but in
/// a marked directory an ignore mark on an emptied file lets every opener
/// through to its zeros. So this walk clears the ignore mark of every file
/// it walks, after marking that file's directory.
///
/// It clears the files it *lists*, and that is all it can promise. A file
/// renamed from a directory the walk has not reached into one it has already
/// passed is never cleared (the final re-review's N2, measured: a
/// stale-marked, emptied file moved up into the root during the walk kept
/// its mark, and its reader got 65 536 zero bytes). So nothing may rest on
/// "resuming interception is a clean slate". What keeps an emptied file
/// from carrying a mark is the daemon's local rule at every punch (Ruling
/// H146): with a link it has the helper clear the mark first, and with none
/// it punches only when no helper is bound to the socket at all. This walk
/// catches what reaches it anyway — a mark carried in from somewhere the
/// rule never saw — which is why it stays.
///
/// Measured in the VM suite before this walk cleared anything:
/// `unregistered_ignore_mark` and `inflight_across_forget`, each a folder
/// registered without interception, a file freed up there with no
/// `ClearIgnore` — as the daemon then did — and the folder registered with
/// interception again: the reader got 65 536 zero bytes after no fetch, on
/// Btrfs, ext4 and XFS. `carried_in_ignore_mark` still measures the walk on
/// its own.
///
/// A mark placed *during* this walk, behind it, is placed by a helper that
/// has just read the file `hydrated` and read it again after marking
/// (`main.rs`, `mark_while_hydrated`): it is a correct mark, not a stale
/// one. The cost is one `fanotify_mark` per file — a lookup of its name —
/// on top of one per directory; a fresh group, at startup, has nothing to
/// clear, and every call then simply answers `ENOENT`.
///
/// Every descent is an `openat2` from the descriptor of the directory being
/// walked, with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS
/// | RESOLVE_NO_XDEV` (spec §6.3). That is not belt and braces: the helper
/// walks as root, so re-opening each child by its full path — resolved afresh,
/// through components an unprivileged owner can replace between the check and
/// the open — is how a user gets `FAN_OPEN_PERM` put on every directory on the
/// machine by pointing a registered root at a symlink. Resolution that cannot
/// leave the directory it started in, cannot follow a symlink and cannot
/// cross a mount point removes the whole class.
pub fn walk_and_mark(marks: &Marks, root: BorrowedFd<'_>, label: &str) -> WalkReport {
    walk_tree(marks, root, label, Action::Mark)
}

/// Takes the permission mark off every directory under `root`, the exact
/// inverse of [`walk_and_mark`] over the same tree (Ruling H58).
///
/// A root that is unregistered but still marked is worse than one that was
/// never registered: its opens are still intercepted, and the helper then has
/// no daemon to ask — `wait_for_daemon` refuses at once for a uid with no
/// registered root — so every placeholder in the tree is answered `EIO`.
/// Unregistering must make a tree uninteresting, not unreadable.
///
/// Failing to remove one mark never stops the walk, and never stops it
/// descending either: a directory that kept its mark is precisely a directory
/// whose children still need theirs removed. `unmark_dir` already treats
/// `ENOENT` — a mark that was never there, or that the kernel evicted — as
/// success.
///
/// # The files' ignore marks go too
///
/// Taking the directory marks off is not the whole inverse. Every file that
/// was hydrated while the root was registered carries an ignore mark of its
/// own, and that mark belongs to this helper's group, not to the
/// registration: it outlives the unregistration for as long as the inode
/// stays in cache. Measured (`tests/vm/scenarios.rs`,
/// `unregistered_ignore_mark`), on Btrfs, ext4 and XFS alike: a folder
/// unregistered, registered again without interception, a file in it
/// dehydrated with no helper link — so no `ClearIgnore` was ever sent — and
/// the folder registered with interception once more. The file was empty,
/// `online-only`, and still ignore-marked; its open raised no event, nothing
/// was fetched, and the reader got 65536 bytes of zeros. That is invariant
/// M3's one silent, unrecoverable failure, reached without any `ClearIgnore`
/// failing.
///
/// So this walk also clears the ignore mark on every regular file it meets,
/// by name ([`Marks::clear_ignore_at`]), after its directory has lost its own
/// mark. A file that cannot be cleared is reported like a directory that
/// could not be unmarked, and does not stop the walk.
pub fn walk_and_unmark(marks: &Marks, root: BorrowedFd<'_>, label: &str) -> WalkReport {
    walk_tree(marks, root, label, Action::Unmark)
}

/// Which direction a walk runs in. The traversal is identical either way —
/// what differs is the call made on each directory and whether a failure
/// stops the descent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Mark,
    Unmark,
}

impl Action {
    fn apply(self, marks: &Marks, dir: BorrowedFd<'_>) -> io::Result<()> {
        match self {
            Action::Mark => marks.mark_dir(dir),
            Action::Unmark => marks.unmark_dir(dir),
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Action::Mark => "mark",
            Action::Unmark => "unmark",
        }
    }

}

fn walk_tree(marks: &Marks, root: BorrowedFd<'_>, label: &str, action: Action) -> WalkReport {
    let mut report = WalkReport::default();
    match action.apply(marks, root) {
        Ok(()) => report.marked += 1,
        Err(e) => report.failures.push(format!("{label}: cannot {}: {e}", action.verb())),
    }
    match root.try_clone_to_owned() {
        Ok(owned) => walk_below(marks, owned, label, 0, action, &mut report),
        Err(e) => report.failures.push(format!("{label}: cannot duplicate the root fd: {e}")),
    }
    report
}

fn walk_below(
    marks: &Marks,
    dir: OwnedFd,
    label: &str,
    depth: usize,
    action: Action,
    report: &mut WalkReport,
) {
    if depth >= MAX_DEPTH {
        report.failures.push(format!("{label}: deeper than {MAX_DEPTH} levels, not walked"));
        return;
    }
    let mut handle = match Dir::from_fd(dir) {
        Ok(handle) => handle,
        Err(e) => {
            report.failures.push(format!("{label}: cannot list: {e}"));
            return;
        }
    };
    // The names are collected before anything is opened: `Dir::iter` borrows
    // the handle mutably, and the handle is what every `openat2` below
    // resolves against.
    let mut names: VecDeque<CString> = VecDeque::new();
    let mut files: Vec<CString> = Vec::new();
    for entry in handle.iter() {
        match entry {
            Ok(entry) => {
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                // `d_type` is only a hint — some filesystems report
                // `Type::Unknown` for everything — so it is used to skip work,
                // never to decide. Anything that is not definitely a
                // non-directory is offered to `openat2` with `O_DIRECTORY`,
                // which settles it in the kernel, without a race, and without
                // ever opening a file (an `ENOTDIR` open raises no fanotify
                // event because no file is opened).
                match entry.file_type() {
                    Some(nix::dir::Type::Directory) | None => {}
                    Some(nix::dir::Type::File) => {
                        files.push(name.to_owned());
                        continue;
                    }
                    Some(_) => continue,
                }
                names.push_back(name.to_owned());
            }
            Err(e) => {
                report.failures.push(format!("{label}: cannot read an entry: {e}"));
                break;
            }
        }
    }

    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(
            ResolveFlag::RESOLVE_BENEATH
                | ResolveFlag::RESOLVE_NO_SYMLINKS
                | ResolveFlag::RESOLVE_NO_MAGICLINKS
                | ResolveFlag::RESOLVE_NO_XDEV,
        );

    // By the time this runs the directory itself has already had `action`
    // applied by the caller: for a registration its files are covered before
    // their ignore marks come off, so each is intercepted from the moment it
    // is cleared; for an unregistration they are no longer covered.
    for name in &files {
        clear_file(marks, handle.as_fd(), name, label, report);
    }

    while let Some(name) = names.pop_front() {
        let shown = format!("{label}/{}", name.to_string_lossy());
        let child = match openat2(handle.as_fd(), name.as_c_str(), how) {
            Ok(child) => child,
            // Not a directory after all: a file whose `d_type` was unknown,
            // or a name that changed since it was listed. It is handled the
            // way a listed file is, and `FAN_MARK_DONT_FOLLOW` keeps that
            // safe if it is really a symlink.
            Err(Errno::ENOTDIR) => {
                clear_file(marks, handle.as_fd(), &name, label, report);
                continue;
            }
            // A symlink, a mount point, or gone between listing and opening:
            // all ordinary, none a failure.
            Err(Errno::ELOOP) | Err(Errno::EXDEV) | Err(Errno::ENOENT) => continue,
            Err(e) => {
                report.failures.push(format!("{shown}: cannot open: {e}"));
                continue;
            }
        };
        // Marked before descending, so nothing created inside it while we are
        // in there can slip past uncovered (invariant M1).
        match action.apply(marks, child.as_fd()) {
            Ok(()) => report.marked += 1,
            Err(e) => {
                report.failures.push(format!("{shown}: cannot {}: {e}", action.verb()));
                // A directory we could not mark is one whose children we have
                // no business marking either — they would be covered by a
                // parent that is not. Removal is the opposite case: a mark we
                // failed to take off is exactly a reason to keep going down.
                if action == Action::Mark {
                    continue;
                }
            }
        }
        walk_below(marks, child, &shown, depth + 1, action, report);
    }
}

/// Takes the ignore mark off one non-directory, in either direction of walk
/// (Ruling H138 for a registration, H132 for an unregistration).
fn clear_file(marks: &Marks, dir: BorrowedFd<'_>, name: &CStr, label: &str, report: &mut WalkReport) {
    if let Err(e) = marks.clear_ignore_at(dir, name) {
        report.failures.push(format!(
            "{label}/{}: cannot clear the ignore mark: {e}",
            name.to_string_lossy()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M2's set, and the errnos the daemon will actually report that are not
    /// in it. A value outside the set is not a curiosity: `ENOENT` is what a
    /// deleted OneDrive item looks like and `ETIMEDOUT` is what a slow network
    /// looks like, and either one reaching `write()` unclamped leaves every
    /// waiting opener suspended for the lifetime of the helper.
    #[test]
    fn accepted_errnos_pass_through_unchanged() {
        for errno in ACCEPTED_DENY_ERRNOS {
            assert_eq!(clamp_deny_errno(errno), errno, "errno {errno} is accepted by the kernel");
        }
    }

    #[test]
    fn everything_else_becomes_eio() {
        for errno in [
            libc::ENOENT,
            libc::EACCES,
            libc::ECONNRESET,
            libc::ENETDOWN,
            libc::ETIMEDOUT,
            libc::ECANCELED,
            libc::EINVAL,
            libc::ENOMEM,
            libc::EEXIST,
            libc::ENODEV,
            libc::EPIPE,
            libc::EHOSTUNREACH,
        ] {
            assert_eq!(clamp_deny_errno(errno), libc::EIO, "errno {errno} must be downgraded");
        }
    }

    /// A daemon is not trusted to send a sensible number at all, and the
    /// response word only has a byte to put it in.
    #[test]
    fn nonsense_values_become_eio_and_never_corrupt_the_response() {
        for errno in [-1, -4095, i32::MIN, i32::MAX, 256, 512, 0x100, 0xdead_beefu32 as i32] {
            let clamped = clamp_deny_errno(errno);
            assert_eq!(clamped, libc::EIO, "errno {errno} must be downgraded");
        }
        // The masking is what guarantees the invariant even if the clamp were
        // ever widened: only the low byte can reach the response word.
        for errno in [-1i32, 0x1234, i32::MIN] {
            assert_eq!((errno as u32 & 0xff) << 24 & !0xff00_0000, 0);
        }
    }

    /// `ClearIgnore` and `UnmarkDir` run against marks the kernel is free to
    /// have thrown away already; only that one errno is normal, and the rest
    /// must still be reported.
    #[test]
    fn a_missing_mark_is_not_a_failure_but_other_errors_still_are() {
        assert!(tolerate_missing_mark(Ok(())).is_ok());
        assert!(tolerate_missing_mark(Err(Errno::ENOENT)).is_ok());
        let error = tolerate_missing_mark(Err(Errno::EBADF)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
