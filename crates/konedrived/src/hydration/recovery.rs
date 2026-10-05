use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{read_state, State, StateError};
use konedrive_fs::{proc_path, MAX_DEPTH};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use crate::helper::{Clearance, HelperError, NotCleared};
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::hydration::demote::{demote, Demoted, FileTimes, Held, Keep, Shape};
use crate::folder::root::SyncRoot;

/// How much of a registered root a startup [`recover`] found, fixed, and
/// could not reach.
///
/// `{ reset, scanned }` alone could not tell "nothing needed fixing" from
/// "every single file was refused", and the second of those leaves files in
/// `dehydrating` — invariant M3's dangerous state — waiting for a next start
/// that may never come. The four counts below are what a caller needs to see
/// that, and every file the walk meets lands in exactly one of them (or in
/// none, when it is simply not ours).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Files carrying a `user.konedrive.state` this daemon wrote: everything
    /// it manages, in whatever state the crash left it. A file with no state
    /// xattr at all is not ours and is not counted here.
    pub scanned: usize,
    /// Interrupted files (`hydrating` or `dehydrating`) that were punched
    /// back to `online-only`. A subset of `scanned`.
    pub reset: usize,
    /// Interrupted files that could **not** be reset: the helper refused the
    /// `ClearIgnore`, or the punch itself failed. Each is left exactly as it
    /// was found, for the next start to try again. A subset of `scanned`,
    /// disjoint from `reset` and `busy`.
    pub failed: usize,
    /// Things recovery could not look at at all: a directory it could not
    /// open or list, a file it could not open, a file whose state xattr it
    /// could not read, a subtree on another filesystem, or a branch deeper
    /// than [`MAX_DEPTH`]. Anything counted here may be hiding an
    /// interrupted file, so a non-zero value means the root was **not**
    /// fully recovered.
    pub skipped: usize,
    /// Interrupted files something had open, so no lease could be taken.
    /// Not a failure: whatever has the
    /// file open is, as often as not, an opener waiting for it to be filled
    /// — or, after a reconnect, a fill from the connection before, still
    /// running — and an interrupted file is one the helper
    /// fills on its next open. Left exactly as found, like a failure, for the
    /// next start if nothing fills it first. A subset of `scanned`.
    pub busy: usize,
    /// Interrupted files left exactly as found because a helper is running
    /// and this daemon has no link to it yet, so a mark its group may hold
    /// on them cannot be cleared. Not a failure: recovery runs
    /// again once the link is up (`SyncService::resume`). A subset of
    /// `scanned`.
    pub deferred: usize,
}

/// Why a whole [`recover`] could not run. Everything smaller than this —
/// one unreadable directory, one file that could not be opened, one
/// `ClearIgnore` the helper refused — is logged and counted in the
/// [`RecoveryReport`] instead, because recovery is the one component whose
/// entire job is coping with a messy on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("{0} no longer carries this sync root's registration")]
    NotRegistered(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Why one interrupted file could not be reset. The kind is preserved rather
/// than flattened into a string, so a caller (and a log reader) can tell a
/// helper that refused from a helper that timed out from a disk that is
/// full — three situations with three different answers.
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error("the helper did not clear the ignore mark: {0}")]
    Helper(HelperError),
    /// A helper is running and there is no link to it.
    #[error("a konedrive helper is running and this daemon is not connected to it yet")]
    Unlinked,
    /// Under the lease the file no longer reads `hydrating` or
    /// `dehydrating` — something finished it after recovery first looked.
    #[error("the file was finished meanwhile; it now reads {0:?}")]
    Finished(Option<State>),
    #[error("the file is open in another process")]
    InUse,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<NotCleared> for ResetError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Helper(e) => ResetError::Helper(e),
            NotCleared::Unlinked => ResetError::Unlinked,
            NotCleared::Unknown(why) => ResetError::Io(io::Error::other(why)),
            // A fill's refusal, never an answer of `Clearance::clear`.
            NotCleared::NoWay => ResetError::Helper(HelperError::NotRunning),
        }
    }
}

/// Startup recovery: after a crash or power loss, a file caught
/// mid-hydration or mid-dehydration holds content that must not be trusted —
/// punch it back to `online-only` so the next open fetches it again.
///
/// # Why this takes a [`Clearance`]
///
/// A file that crashed `dehydrating` can still be carrying its ignore mark:
/// the daemon may have died between `write_state(Dehydrating)` and a
/// successful `ClearIgnore`. That mark carries
/// `FAN_MARK_IGNORED_SURV_MODIFY` (`docs/design/hydration.md` §4.3), so
/// nothing clears it on its own any more, and punching a file that still
/// carries it reproduces the exact defect `dehydrate` in this module exists
/// to prevent (invariant M3): the file ends up empty *and* permanently
/// un-intercepted, reading as zeros on every future open with no error
/// anywhere to notice it. So every file this function is about to punch goes
/// through the same local rule `dehydrate` applies (on
/// [`Clearance`]) — on the same descriptor it is about to punch, never a
/// path re-open — and a file the rule does not clear is left
/// exactly as it was found: counted `failed` when the helper refused, and
/// `deferred` when a helper is running that this daemon has no link to,
/// until the link is up. This needs no special case for a mark that was
/// never there or that the kernel already evicted: the helper acks that as
/// success too (see `dehydrate`'s doc comment).
///
/// For an intercepted root the caller passes the link it has just
/// registered the root on — callers must connect to the helper and register
/// their roots before recovering them, not after. For a root
/// registered without interception the caller passes whatever the rule has
/// to go on: its link if it has one, the helper's socket if not.
///
/// # The file can be finished while recovery looks at it
///
/// Recovery runs on every reconnect, and the previous connection's fills
/// keep running while it walks. It read a file's state once, when it
/// opened it, and nothing stopped a fill from committing
/// `hydrated` — and an opener from having the helper ignore-mark the file —
/// between recovery's `ClearIgnore` and its lease: recovery then punched a
/// complete file under a fresh mark, and the next reader got 65 536 zero
/// bytes (measured with that gap widened). So,
/// for each interrupted file:
///
/// - the per-inode lock every fill and every free-up of this daemon holds
///   (`locks`, `SyncService`'s own) is taken, and if it is held the file is
///   left as it is and counted `busy` — something is filling it or freeing
///   it up right now. Taken without waiting: waiting would hold the reconnect
///   behind a download of any length;
/// - the state is read **again once the lease is held**, and the file is
///   punched only if it still reads `hydrating` or `dehydrating`. Under the
///   lease nothing else has the file open, so no fill is under way; and a
///   file that came back to one of those two states after reading
///   `hydrated` did so through a free-up or a fill that cleared its mark
///   first (M3), and cannot be marked again while it reads them. A file that
///   reads anything else was finished meanwhile, and is left as it is.
///
/// # The walk never leaves the root, by construction
///
/// This empties files, in bulk, across a whole tree, with no user pointing
/// at any of them — so the containment `SyncRoot::open_inside` gives
/// `dehydrate` matters more here, not less. It takes the `&SyncRoot` rather
/// than a path for exactly that reason, and:
///
/// - the root directory must still carry *this* root's `user.konedrive.root`
///   before anything inside it is touched at all ([`RecoveryError::NotRegistered`]);
/// - every step of the walk is an `openat` from a **directory descriptor**,
///   never a path — `O_DIRECTORY | O_NOFOLLOW` for subdirectories, `O_RDONLY
///   | O_NOFOLLOW` for files, reopened writable through the descriptor itself
///   only for the one being reset — so the name that was classified is the
///   name that is opened, out of a directory that cannot be swapped
///   underneath the walk while it waits for the helper;
/// - every descriptor is `fstat`ed after it is opened and refused unless it
///   is a regular file (or a directory) on the root's own `st_dev`.
///
/// The version this replaces re-resolved every subdirectory by absolute path
/// with `std::fs::read_dir` and opened every file with `File::open`, both of
/// which follow symlinks, having classified the entry with an lstat-shaped
/// `DirEntry::file_type` an unbounded time earlier — the gap is a helper
/// round trip per interrupted file. Reproduced: with `sub/`
/// replaced by a symlink to a directory outside the root while recovery
/// awaited a `ClearIgnore` ack, a file outside the root was emptied,
/// relabelled `online-only` and counted as a success. The helper does not
/// back this out: its own check is "same uid, same *filesystem* as one of
/// that uid's roots", which any file in the user's home satisfies.
///
/// # One file open at a time
///
/// The walk opens a file, decides about it, punches it and closes it before
/// it opens the next one. The version this replaces opened *every* regular
/// file in a directory `O_RDWR` — whatever its state — and held all of those
/// descriptors until the directory was finished, with `Err(_) => continue`
/// turning the inevitable `EMFILE` into silence: measured, 2000 interrupted
/// files under the default `RLIMIT_NOFILE` of 1024 gave
/// `RecoveryReport { reset: 1009, scanned: 1009 }` and left 991 files
/// `hydrating` with untrusted content, no error and no log line. What the
/// walk does hold is one directory descriptor per *level* of the tree it is
/// currently inside, which is bounded by the depth of the tree rather than
/// by the size of any directory, and a directory that cannot be opened is
/// counted in `skipped` rather than passed over in silence.
pub async fn recover(
    clearance: &Clearance,
    root: &SyncRoot,
    locks: &InodeLocks,
) -> Result<RecoveryReport, RecoveryError> {
    let opened = root.clone();
    let root_dir = on_blocking_thread(move || opened.open_registered())
        .await??
        .ok_or_else(|| RecoveryError::NotRegistered(root.path.clone()))?;
    let root_dev = root_dir.metadata()?.dev();

    let mut report = RecoveryReport::default();
    let mut stack = Vec::new();
    descend(&mut stack, Arc::new(root_dir), root.path.clone(), 0, &mut report).await;

    while let Some(mut frame) = stack.pop() {
        let Some((name, kind)) = frame.names.next() else {
            // Finished: the directory descriptor goes with the frame.
            continue;
        };
        let dir = Arc::clone(&frame.dir);
        let shown = frame.shown.join(&name);
        let child_depth = frame.depth + 1;
        // Back on the stack before anything is awaited, so the walk resumes
        // in this directory — through this descriptor — afterwards.
        stack.push(frame);

        let opened = on_blocking_thread(move || open_entry(&dir, &name, kind, root_dev)).await?;
        match opened {
            Err(e) => {
                report.skipped += 1;
                tracing::warn!("startup recovery: cannot open {}: {e}", shown.display());
            }
            Ok(Entry::Elsewhere) => {}
            Ok(Entry::OtherFilesystem) => {
                report.skipped += 1;
                tracing::warn!(
                    "startup recovery: {} is on a different filesystem than the root and was \
                     not recovered",
                    shown.display()
                );
            }
            Ok(Entry::Directory(sub)) => {
                descend(&mut stack, Arc::new(sub), shown, child_depth, &mut report).await;
            }
            Ok(Entry::File(file, state)) => {
                recover_file(clearance, locks, file, state, &shown, &mut report).await;
            }
        }
    }
    Ok(report)
}

/// One directory the walk is part-way through: the descriptor everything
/// inside it is opened from, and the names still to look at.
///
/// Names, not descriptors: a directory of 100 000 files costs one `Vec` of
/// its names, while opening them up front would cost 100 000 descriptors.
/// The `FileType` beside each name is the `d_type` the kernel
/// gave us — a hint for which of the two opens to attempt, never the
/// authority for what was opened, which is the `fstat` in [`open_entry`].
struct Frame {
    dir: Arc<File>,
    /// The path a person would recognise, for log lines only. Nothing is
    /// ever opened through it.
    shown: PathBuf,
    names: std::vec::IntoIter<(OsString, std::fs::FileType)>,
    /// This directory's nesting level under the root, root itself being 0 —
    /// the same convention the helper's `walk_below` uses for `MAX_DEPTH`.
    depth: usize,
}

/// Lists `dir` and pushes it onto the walk. A directory that cannot be
/// listed is counted and left behind rather than ending the walk: a
/// mode-`000` subdirectory, or one the user deleted while the daemon
/// was starting, used to make `recover` return `Err` for the *whole* root,
/// losing the count of what it had already punched and never visiting a
/// sibling subtree.
async fn descend(
    stack: &mut Vec<Frame>,
    dir: Arc<File>,
    shown: PathBuf,
    depth: usize,
    report: &mut RecoveryReport,
) {
    if depth >= MAX_DEPTH {
        // The helper's own walk stopped marking at this depth — the same
        // `konedrive_fs::MAX_DEPTH` — so anything under here is unmarked and
        // cannot be intercepted regardless of what recovery finds in it. Counted, not
        // silent: `skipped`'s own doc comment promises that.
        report.skipped += 1;
        tracing::warn!(
            "startup recovery: {} is deeper than {MAX_DEPTH} levels, matching the helper's own \
             limit — not walked further",
            shown.display()
        );
        return;
    }
    let listing = {
        let dir = Arc::clone(&dir);
        on_blocking_thread(move || list_names(&dir)).await
    };
    match listing {
        Ok(Ok((names, unreadable))) => {
            report.skipped += unreadable;
            if unreadable > 0 {
                tracing::warn!(
                    "startup recovery: {unreadable} entries of {} could not be read",
                    shown.display()
                );
            }
            stack.push(Frame { dir, shown, names: names.into_iter(), depth });
        }
        Ok(Err(e)) | Err(e) => {
            report.skipped += 1;
            tracing::warn!(
                "startup recovery: cannot read {}, so nothing inside it is recovered this \
                 time: {e}",
                shown.display()
            );
        }
    }
}

/// Every name in an open directory, with the entry kind the kernel reported
/// for it, plus a count of the entries that could not be read at all.
///
/// The directory is listed through `/proc/self/fd/<n>`, so the listing is of
/// the descriptor the walk holds — not of whatever the path may name by now.
fn list_names(dir: &File) -> io::Result<(Vec<(OsString, std::fs::FileType)>, usize)> {
    let mut names = Vec::new();
    let mut unreadable = 0;
    for entry in std::fs::read_dir(proc_path(dir))? {
        match entry.and_then(|entry| Ok((entry.file_name(), entry.file_type()?))) {
            Ok(entry) => names.push(entry),
            Err(_) => unreadable += 1,
        }
    }
    Ok((names, unreadable))
}

/// What one name in a directory turned out to be, once opened.
enum Entry {
    /// A directory on the root's own filesystem, to walk into.
    Directory(File),
    /// A regular file on the root's own filesystem, open read-only, with
    /// whatever `user.konedrive.state` it carries — including the error of
    /// failing to make sense of it, which is emphatically not the same as
    /// carrying none (see `konedrive_fs::placeholder::StateError`).
    File(File, Result<Option<State>, StateError>),
    /// Something startup recovery has no business opening for writing: a
    /// symlink, a FIFO, a socket or a device node. None of it is ours, and
    /// none of it is worth a log line — a directory listing full of a
    /// user's ordinary files is expected to contain exactly this kind of
    /// thing, and recovery not commenting on every one of them is the point.
    Elsewhere,
    /// A directory or file `fstat`-confirmed to be on a filesystem other
    /// than the root's own `st_dev` — a bind mount or a removable disk
    /// mounted inside the sync folder. Unlike [`Entry::Elsewhere`] this is
    /// not silent: the helper's own validation is scoped to the root's
    /// device, so punching across one is exactly the escape it would not
    /// catch, and a whole subtree excluded this way could be
    /// hiding an interrupted file recovery never looked at — precisely what
    /// `RecoveryReport::skipped`'s doc comment says a non-zero value means.
    OtherFilesystem,
}

/// Opens one name **from the directory descriptor it was listed in**, with
/// the flags its kind calls for, and then proves on the open descriptor what
/// it is.
///
/// `kind` is only the `d_type` from the listing: it decides which of the two
/// opens to attempt, and is never trusted for anything after that. A name
/// that was a regular file when it was listed and is a symlink by the time
/// it is opened is refused by `O_NOFOLLOW` (`ELOOP`); one that has become a
/// directory is refused by the `fstat` below; one that has become a FIFO or
/// a device node is opened once, read-only, and refused by the same `fstat`
/// before anything at all is read from it or written to it. (`O_NONBLOCK`
/// keeps a read-only open of a FIFO from waiting for a writer, and an
/// unprivileged process cannot create a device node inside a sync root to
/// begin with.)
///
/// Files are opened **read-only**: under the read
/// phase's lock every file is `0444`, and `O_RDWR` would refuse them all —
/// every interrupted file counted `skipped` and never reset. Nothing is
/// written through this descriptor; `reset_interrupted` reopens the one file
/// it is about to reset writable, on the same inode, so a file that is not
/// ours is never made writable, not even for the moment of an open.
fn open_entry(
    dir: &File,
    name: &OsString,
    kind: std::fs::FileType,
    root_dev: u64,
) -> io::Result<Entry> {
    let flags = if kind.is_dir() {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else if kind.is_file() {
        // Read-only: under the lock every file is
        // `0444`, and the one file recovery is about to reset is reopened
        // writable in `reset_interrupted` — never a file that is not ours.
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else {
        return Ok(Entry::Elsewhere);
    };
    let opened = match nix::fcntl::openat(dir.as_fd(), name.as_os_str(), flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        // The name stopped being what it was between the listing and here:
        // a symlink now (`ELOOP`), or no longer a directory (`ENOTDIR`, also
        // what `O_DIRECTORY | O_NOFOLLOW` reports for a symlink). Neither is
        // ours to open, and neither is a reason to stop.
        Err(Errno::ELOOP | Errno::ENOTDIR | Errno::EISDIR) => return Ok(Entry::Elsewhere),
        Err(e) => return Err(e.into()),
    };

    let meta = opened.metadata()?;
    if meta.dev() != root_dev {
        return Ok(Entry::OtherFilesystem);
    }
    if meta.is_dir() {
        return Ok(Entry::Directory(opened));
    }
    if !meta.is_file() {
        return Ok(Entry::Elsewhere);
    }
    let state = read_state(&opened);
    Ok(Entry::File(opened, state))
}

/// What to do about one open file: nothing at all unless it is one of ours
/// and the crash caught it mid-operation.
async fn recover_file(
    clearance: &Clearance,
    locks: &InodeLocks,
    file: File,
    state: Result<Option<State>, StateError>,
    shown: &Path,
    report: &mut RecoveryReport,
) {
    let state = match state {
        // Not a file this daemon manages. Not ours to count, and certainly
        // not ours to empty.
        Ok(None) => return,
        Ok(Some(state)) => state,
        // A managed file whose state we cannot make sense of, or cannot read
        // at all. Recovery must not punch it — `unwrap_or(None)` would have
        // called it "not one of ours" and moved on — but it must not pass
        // over it in silence either: §5.2 has the helper deny every open of
        // such a file with `EIO` for as long as it stays that way, and
        // recovery is the one place that walks the whole tree and could
        // notice.
        Err(e) => {
            report.skipped += 1;
            tracing::error!(
                "startup recovery: cannot read the state of {}: {e}. It is left exactly as it \
                 is — the helper denies every open of a managed file in an unknown state — but \
                 nothing here can repair it",
                shown.display()
            );
            return;
        }
    };
    report.scanned += 1;
    if !matches!(state, State::Hydrating | State::Dehydrating) {
        return;
    }
    // Not while this daemon fills or frees up the same file.
    let _guard = match InodeKey::of(&file) {
        Ok(key) => match locks.try_lock(key) {
            Some(guard) => guard,
            None => {
                report.busy += 1;
                tracing::info!(
                    "startup recovery: {} is {state:?} and being filled or freed up right now; \
                     left as it is",
                    shown.display()
                );
                return;
            }
        },
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: cannot tell which file {} is ({e}); left as found",
                shown.display()
            );
            return;
        }
    };
    match reset_interrupted(clearance, file).await {
        Ok(()) => report.reset += 1,
        Err(ResetError::Finished(now)) => {
            tracing::info!(
                "startup recovery: {} was {state:?} and is {now:?} now — finished while \
                 recovery looked at it; left as it is",
                shown.display()
            );
        }
        Err(ResetError::Unlinked) => {
            report.deferred += 1;
            tracing::info!(
                "startup recovery: {} is {state:?}, and a konedrive helper is running that this \
                 daemon is not connected to yet, so a mark it may hold on the file cannot be \
                 cleared; left as found until the connection is up",
                shown.display()
            );
        }
        Err(ResetError::InUse) => {
            report.busy += 1;
            tracing::info!(
                "startup recovery: {} is open elsewhere, {state:?}; left as found — its next \
                 open fills it, or the next start resets it",
                shown.display()
            );
        }
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: leaving {} exactly as found, {state:?}, for the next start: \
                 {e}",
                shown.display()
            );
        }
    }
}

/// Clears the ignore mark and punches one crash-interrupted file, both on
/// the inode the walk opened — through one writable reopen of the
/// descriptor the walk opened read-only (`/proc/self/fd/<n>`),
/// made before either — the sequence a free-up runs on a file this process
/// is working on ([`demote`] under the lease), applied here to one a crash
/// left mid-sequence. Nothing here re-opens anything by path: the inode
/// that was classified is the inode that is punched, whatever the name points
/// at by the time the helper answers.
///
/// A `hydrating` file whose download left a checkpoint keeps the
/// checkpointed prefix and the checkpoint; only what lies past it
/// is punched, and the next open continues the download from there.
///
/// Returns before punching, leaving the file untouched, if `ClearIgnore` is
/// refused; that failure is never swallowed (see [`recover`]'s doc comment).
///
/// # The lease, and why recovery needs it more than `dehydrate`
///
/// A free-up takes an `F_SETLEASE` before it empties a file, so
/// that an application which opens it mid-punch is suspended by the kernel
/// instead of reading a file with its blocks going away underneath. The
/// window is *wider* at startup, not narrower: the helper's own
/// `register_root` walk has to mark the whole tree before anything is
/// intercepted at all, so until that finishes any thumbnailer, backup or
/// indexer can have a `hydrating`/`dehydrating` file open while this runs.
/// A refused lease means exactly that — somebody has it open — so the file
/// is left as it was found and counted, for a start that finds it quieter.
///
/// # The mtime
///
/// `fallocate` moves the mtime to now, and §4.2 wants an `online-only`
/// file's mtime to be the remote `lastModifiedDateTime`. Recovery has no
/// remote metadata to restore, so it restores what the file had a moment
/// before the punch — which for a `dehydrating` file is exactly the remote
/// stamp `create_placeholder` set, and for a `hydrating` one is the time the
/// interrupted download last wrote, the best available answer until the next
/// hydration sets it properly. What it must not do is leave *now*: a whole
/// tree of files that recovery touched would then look locally modified, and
/// each is a hole full of zeros, which is an upload-over-remote hazard the
/// moment a delta engine exists. The stamp is removed in the same sequence,
/// so `dehydrate`'s "modified locally" guard is not what saves you.
///
/// In a folder that shows OneDrive, the time of a `hydrating` file kept with
/// its checkpoint is the one thing left wrong, and not for long: every
/// bring-up starts the folder's sync, whose first cycle is a Full reconcile,
/// and that puts the tree's time back without touching the checkpoint
/// (`materialize::check_file`).
async fn reset_interrupted(clearance: &Clearance, file: File) -> Result<(), ResetError> {
    // Writable only now, and only this file: the walk opened
    // every file read-only, and a file that is not ours is never made
    // writable, even for a moment — only a file the walk read `hydrating` or
    // `dehydrating` gets here. The reopen goes through the descriptor, so it
    // is the inode that was classified, whatever the name leads to by now.
    //
    // It comes *before* the clear, and the read-only descriptor is closed at
    // once, so that the mark is cleared on, and the lease taken on, one and
    // the same open file. A write lease is refused while any other open file
    // of the inode exists, and the helper link sends a *duplicate* of the
    // descriptor it is given, which its writer thread drops only after the
    // send — possibly after the `Ack` has already brought this function to
    // its lease. With the read-only descriptor handed to the helper, that
    // duplicate kept the read-only open file alive and the lease was refused:
    // measured, every file `busy` and none reset with the writer thread
    // delayed 50 ms after its send. A duplicate of the descriptor the lease
    // is taken on is the same open file, and refuses nothing.
    let file = on_blocking_thread(move || {
        let writable = konedrive_fs::placeholder::reopen_writable(&file)?;
        drop(file);
        Ok::<_, io::Error>(writable)
    })
    .await??;
    // Invariant M3, on the very descriptor the punch will use — by the local
    // rule (see [`Clearance`]), for a root with interception or
    // without. It used to be skipped for a root without interception, on the
    // strength of a chain of reasoning about where a stale mark could be; the
    // chain was falsified three times, and nothing here
    // depends on it any more.
    clearance.clear(&file).await?;
    // `fault-injection` builds only: the VM suite's N3 scenario.
    fault::recovery_after_clear().await;
    on_blocking_thread(move || {
        let Some(lease) = lease_retrying_briefly(&file)? else {
            return Err(ResetError::InUse);
        };
        // Looked at again inside, now that nothing else has the file open. A
        // download's checkpoint is kept with its bytes: they were made durable
        // before it was written, and the fill that continues from it checks
        // them against the quickXorHash along with the rest.
        let shape = Shape { size: None, times: FileTimes::of(&file)? };
        let demoted = demote(&file, Keep::Checkpoint, shape, Held::Lease(&lease))?;
        drop(lease);
        match demoted {
            Demoted::Done { .. } => Ok(()),
            Demoted::Left(now) => Err(ResetError::Finished(now)),
        }
    })
    .await?
}

/// The write lease recovery punches under, with a refusal retried a few
/// times over about 75 ms before the file counts as in use.
///
/// `F_SETLEASE` is refused while any other open file of the inode exists
/// anywhere, and recovery's walk makes one of its own it cannot fully control:
/// the read-only descriptor it classified the file on, closed before the
/// lease — but a process being spawned at that moment holds an inherited copy
/// of it until its `exec`, and that copy alone refuses the lease. Measured on
/// this module's tests, where one test spawns `unshare`: 2 runs in 10 left an
/// interrupted file `busy`, none in 20 with that test skipped, and none in 30
/// on part 1's code, whose walk and lease shared one open file. Such a copy
/// goes within milliseconds; an application holding the file open does not,
/// and still finds the file `busy` after the last try, as before.
fn lease_retrying_briefly(file: &File) -> io::Result<Option<WriteLease<'_>>> {
    for pause in [5, 20, 50] {
        if let Some(lease) = WriteLease::take(file)? {
            return Ok(Some(lease));
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
    }
    WriteLease::take(file)
}

/// A deliberate stall for a race window too narrow to hit by chance —
/// compiled in **only** with the `fault-injection` cargo feature, like the
/// helper's. `tests/vm/Cargo.toml` builds this crate with it
/// for the VM suite; the daemon that ships does not have it.
///
/// Startup recovery clears a file's ignore mark and then takes its write
/// lease. A fill from the previous
/// helper connection can commit `hydrated` in between, and an opener can have
/// the file ignore-marked, in well under a millisecond; the VM suite widens
/// that gap to put both inside it.
#[cfg(feature = "fault-injection")]
pub mod fault {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static RECOVERY_STALL_MS: AtomicU64 = AtomicU64::new(0);

    /// From now on, every file recovery is about to reset waits this long
    /// between clearing its ignore mark and taking its lease. Zero disarms.
    pub fn set_recovery_stall(stall: Duration) {
        RECOVERY_STALL_MS.store(stall.as_millis() as u64, Ordering::SeqCst);
    }

    pub(super) async fn recovery_after_clear() {
        let ms = RECOVERY_STALL_MS.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

/// The shipped build: nothing to arm, nothing compiled in.
#[cfg(not(feature = "fault-injection"))]
mod fault {
    #[inline(always)]
    pub(super) async fn recovery_after_clear() {}
}

/// Runs one blocking step of the walk on a blocking thread:
/// `openat`, `getxattr`, `fallocate` and `fsync` are all blocking syscalls,
/// and `helper/mod.rs`'s module doc treats a blocking call left on a tokio
/// worker as a first-class defect.
async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(format!("the recovery task failed: {e}")))
}

#[cfg(test)]
mod tests;
