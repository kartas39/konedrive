//! Where hydration gets its bytes from, and the loop that fills a
//! placeholder in place from one of those sources.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use konedrive_fs::placeholder::{
    punch_all, read_item_id, read_state, remove_stamp, stamp_matches, write_state, write_stamp,
    State, XATTR_STATE,
};
use konedrive_proto::clamp_deny_errno;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::helper::{Clearance, HelperLink, NotCleared};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Transient(String),
}

pub struct Fetched {
    /// The offset this stream actually starts at.
    ///
    /// `fill` asks for `from` and writes what comes back at `from`; a source
    /// that silently serves something else corrupts the file and there is no
    /// way to notice after the fact (Ruling H49). This is not a theoretical
    /// implementor: the Graph source issues HTTP `Range` requests, and a
    /// server that answers `200` instead of `206` — which is always allowed —
    /// restarts the body at 0. Reporting the served offset is the only thing
    /// that makes that case detectable, so every implementation must set it
    /// to the offset of the first byte of `stream`, not to the offset it was
    /// asked for.
    pub served_from: u64,
    pub size: u64,
    pub mtime: SystemTime,
    pub stream: Box<dyn AsyncRead + Send + Unpin>,
}

#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Bytes of `item_id` starting at `from`, plus the item's current size and mtime.
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError>;
}

/// Why a source file must not be read into a placeholder (Ruling H148), or
/// `None` if it may be: it is one of konedrive's own files — it carries
/// `user.konedrive.*` attributes, however it was reached (a hardlink, a bind
/// mount) — or the file it leads to lies inside the sync folder `root`.
///
/// A placeholder read through a source is read with nothing intercepting
/// it — or through the daemon's own exemption — so its zeros come back as
/// content, and a fill writes them into the file it fills and stamps it
/// `hydrated`: the outcome this whole component exists to prevent, on the
/// offline route a user actually runs. A source *directory* that overlaps
/// the folder is refused before a populate starts (m10); a source *file*
/// can still lead into it — a symlink to a placeholder there, or a hardlink
/// to one — and that is what this looks at.
///
/// `xattrs` are the file's attribute names, `resolved` where it really is.
fn refused_source(
    xattrs: impl Iterator<Item = std::ffi::OsString>,
    resolved: &Path,
    root: &Path,
) -> Option<String> {
    let ours = xattrs
        .into_iter()
        .any(|name| name.as_encoded_bytes().starts_with(b"user.konedrive."));
    if ours {
        return Some("it is one of konedrive's own files (it carries user.konedrive.* attributes)".into());
    }
    if resolved.starts_with(root) {
        return Some(format!("it is {}, inside the sync folder", resolved.display()));
    }
    None
}

/// [`refused_source`] for a source file named by path, following a
/// symlink as a fill would: the check `PopulateFromDirectory` makes before
/// it mirrors the file. Reads attributes and resolves the path; opens
/// nothing.
pub(super) fn refused_source_path(path: &Path, root: &Path) -> io::Result<Option<String>> {
    let resolved = std::fs::canonicalize(path)?;
    let names = xattr::list_deref(path)?;
    Ok(refused_source(names, &resolved, root))
}

/// Test source: one file per item id in a directory, with fault injection.
pub struct LocalDir {
    dir: PathBuf,
    /// The sync folder this source fills, whose own files it refuses to be
    /// read from (Ruling H148); `None` for a source that fills nothing real.
    refusing: Option<PathBuf>,
    fail_at: Option<u64>,
    /// With `fail_at`, whether the break applies to the first fetch only.
    /// A permanent break can only ever exercise the give-up path; healing
    /// after one failure is what lets a test follow a file all the way
    /// across two fetches, which is where the resume arithmetic lives.
    heal_after_first_failure: bool,
    fetches: AtomicU64,
    delay: Option<std::time::Duration>,
}

impl LocalDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            refusing: None,
            fail_at: None,
            heal_after_first_failure: false,
            fetches: AtomicU64::new(0),
            delay: None,
        }
    }

    /// Refuse to serve any file that leads into `root`, the sync folder
    /// this source fills, or that is one of konedrive's own files (Ruling
    /// H148) — decided on the descriptor the bytes would be read from, so
    /// a symlink swapped after the folder was populated is caught too.
    pub fn refusing_files_of(mut self, root: impl Into<PathBuf>) -> Self {
        self.refusing = Some(root.into());
        self
    }

    /// Break the stream after this many bytes, on every fetch.
    pub fn fail_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = false;
        self
    }

    /// Break the stream after this many bytes on the first fetch only; every
    /// later fetch serves the rest of the file (Ruling H49).
    pub fn fail_once_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = true;
        self
    }

    pub fn delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// How many times `fetch` has been called. A test that means "and it did
    /// not even try again" has to be able to say so.
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ContentSource for LocalDir {
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        let fetch_number = self.fetches.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let path = self.dir.join(item_id);
        // One open, and everything decided on it: what it is, whether it may
        // be read (Ruling H148), and the bytes.
        let opened = std::fs::File::open(&path).map_err(|e| SourceError::NotFound(e.to_string()))?;
        if let Some(root) = &self.refusing {
            let names = xattr::FileExt::list_xattr(&opened)
                .map_err(|e| SourceError::NotFound(format!("{}: {e}", path.display())))?;
            let resolved = std::fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd()))
                .map_err(|e| SourceError::NotFound(format!("{}: {e}", path.display())))?;
            if let Some(why) = refused_source(names, &resolved, root) {
                tracing::error!("refusing to fill a file from {}: {why}", path.display());
                return Err(SourceError::NotFound(format!("{}: {why}", path.display())));
            }
        }
        let meta = opened.metadata().map_err(|e| SourceError::NotFound(e.to_string()))?;
        let mut file = tokio::fs::File::from_std(opened);
        if from > 0 {
            use tokio::io::AsyncSeekExt;
            file.seek(io::SeekFrom::Start(from))
                .await
                .map_err(|e| SourceError::Transient(e.to_string()))?;
        }
        let mut stream: Box<dyn AsyncRead + Send + Unpin> = Box::new(file);
        let breaks_here = match self.fail_at {
            Some(limit) if !self.heal_after_first_failure || fetch_number == 0 => Some(limit),
            _ => None,
        };
        if let Some(limit) = breaks_here {
            stream = Box::new(stream.take(limit.saturating_sub(from)));
        }
        Ok(Fetched {
            served_from: from,
            size: meta.len(),
            mtime: meta.modified().map_err(|e| SourceError::Transient(e.to_string()))?,
            stream,
        })
    }
}

/// Maps a local filesystem failure onto the errno the suspended `open()` is
/// answered with.
///
/// A **clamp**, not a flattening (Ruling H51): `ENOSPC` and `EDQUOT` are in
/// the kernel's accepted `FAN_DENY` set and are exactly what a `pwrite` or an
/// `fsync` produces on a full disk or an exhausted quota — the case §5.2 step
/// 5 and §9 both name. Everything else the local filesystem can report
/// (`EROFS`, `EBADF`, `EFBIG`, ...) is outside the set and would make the
/// helper's response `write()` fail with `EINVAL`, leaving the opener
/// suspended forever, so it becomes `EIO`.
fn errno_of(e: &io::Error) -> i32 {
    clamp_deny_errno(e.raw_os_error().unwrap_or(libc::EIO))
}

/// Why a fill did not happen, or did not finish.
#[derive(Debug)]
pub enum FillError {
    /// It ran and failed; the opener is answered with this errno, and the
    /// file was rolled back (see [`roll_back`]).
    Errno(i32),
    /// It never started (Ruling H146): the file may carry an ignore mark
    /// that could not be cleared, and a fill that fails empties the file.
    /// Nothing was fetched, and the state is back to what it was found in.
    NotCleared(NotCleared),
}

impl FillError {
    /// What a suspended open is answered with.
    pub fn errno(&self) -> i32 {
        match self {
            FillError::Errno(errno) => *errno,
            FillError::NotCleared(_) => libc::EIO,
        }
    }
}

/// Fills a placeholder in place through the event fd. Returns the errno to
/// answer the suspended open with; 0 means "let it through".
///
/// Unconditional: the caller has already decided the file needs filling, and
/// that no ignore mark needs clearing first — see [`hydrate_with`], which
/// this is with no clearance at all. Only a file known to be `online-only`
/// qualifies (see `hydrate_with` for why); the tests are its callers.
///
/// Every value returned here is in `konedrive_proto::ACCEPTED_DENY_ERRNOS`:
/// `FAN_DENY | (errno << 24)` is only accepted by the kernel for that set,
/// and any other value makes the helper's response write fail with `EINVAL`
/// and the suspended `open()` hang forever. Failures that carry no local
/// errno — a missing remote item, a dropped connection, a source that will
/// not resume — are reported as `EIO`, never as the errno their cause might
/// suggest (`ENOENT`, `ECONNRESET`, `ETIMEDOUT`).
pub async fn hydrate(fd: OwnedFd, source: &dyn ContentSource) -> i32 {
    hydrate_with(fd, source, None).await.err().map_or(0, |e| e.errno())
}

/// What an intercepted open's hydration request does once it holds the
/// per-inode lock: **look again** (spec §5.3, Ruling H137), and fill only a
/// file that still needs it.
///
/// A request can wait a long time before it gets here — for one of the four
/// fill slots behind other downloads, or in the helper for credit — and the
/// file it names can have been filled meanwhile: by `Hydrate()`, or because a
/// `Dehydrate` that the open itself made fail (its suspended descriptor
/// refuses the lease) rolled the file back to `hydrated`. Filling it again
/// without looking was the final review's C1. The re-fill wrote `hydrating`
/// over a hydrated file and fetched; a failed fetch rolled back — demoted and
/// punched — a file that an opener in between had found `hydrated` and had
/// the helper ignore-mark, and the next reader got 65 536 zero bytes without
/// being intercepted at all (measured on Btrfs, ext4 and XFS, nothing
/// injected). A re-fetch that succeeded overwrote any edit made in place
/// since, which is data loss of the other kind.
///
/// So a file that reads `hydrated` is answered 0 and not touched, whatever
/// its stamp says. With a matching stamp it is simply there; with a stamp
/// that does not match it was edited locally, and that edit is the only copy
/// (§8); with none it was labelled by something other than this daemon — the
/// case `Hydrate()` repairs on request (H109), and never something to do
/// behind an opener's back, since the helper lets every opener of a
/// `hydrated` file through anyway. The helper reads the state again itself
/// before it lets the opener through (§5.2 step 5).
pub async fn answer_request(
    fd: OwnedFd,
    source: &dyn ContentSource,
    link: Option<&HelperLink>,
) -> i32 {
    let file = File::from(fd);
    match read_state(&file) {
        Ok(Some(State::Hydrated)) => {
            let edited = !matches!(stamp_matches(&file), Ok(true));
            tracing::info!(
                "a hydration request found its file already hydrated — filled while the request \
                 waited{} — and answers it without touching the file",
                if edited { ", and changed since (or never stamped): its content is kept" } else { "" }
            );
            0
        }
        Ok(Some(_)) => {
            let clearance = link.map(|link| Clearance::Link(link.clone()));
            fill_file(file, source, clearance.as_ref()).await.err().map_or(0, |e| e.errno())
        }
        Ok(None) => {
            tracing::error!(
                "a hydration request names a file with no konedrive state; it is not ours to fill"
            );
            libc::EIO
        }
        Err(e) => {
            tracing::error!("cannot read the state of a file a hydration request names: {e}");
            libc::EIO
        }
    }
}

/// [`hydrate`], clearing the file's ignore mark first when it could be
/// carrying one.
///
/// # Can this file carry a mark placed after the last clear?
///
/// That is the question every punch has to answer (the final review's
/// recommendation 3), and a fill can punch: [`roll_back`] empties the file
/// when the fill fails. The helper places an ignore mark only on a file it
/// has read `hydrated` — and reads again, after placing it (Ruling H139) —
/// so which state the fill starts from decides the answer:
///
/// - `online-only`: no. An open of it raised an event, so it carried no
///   mark then, and none can be placed once this fill has written
///   `hydrating`. (A stale mark on an `online-only` file already reads
///   zeros; filling the file repairs that, and failing leaves it as it was.)
/// - `hydrated` (only `Hydrate()` fills one, when its stamp is missing, H109)
///   and `dehydrating` (a `Dehydrate` cancelled between its state write and
///   its `ClearIgnore`, Ruling N3): yes. `hydrating`, which a panicked or
///   crashed fill of such a file leaves: possibly.
///
/// For those, `hydrating` is made durable first and then the way is cleared
/// by Ruling H146's local rule ([`Clearance`]) — the helper asked to
/// `ClearIgnore`, in the order §8 uses, so that an opener's mark placed in
/// between is found and taken off again by the helper's own re-read; or, with
/// no link, the helper's socket looked at — before the first byte is
/// fetched. If the way is not cleared, nothing is fetched, the state it was
/// found in is put back, and the answer is [`FillError::NotCleared`].
///
/// `clearance` is `None` only where the caller knows the file to be
/// `online-only` — the case above that needs nothing — and never on the
/// strength of the folder's mode: a folder without interception is cleared
/// like any other (H146).
pub async fn hydrate_with(
    fd: OwnedFd,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
) -> Result<(), FillError> {
    fill_file(File::from(fd), source, clearance).await
}

async fn fill_file(
    file: File,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
) -> Result<(), FillError> {
    let original_size = file.metadata().map_err(|e| FillError::Errno(errno_of(&e)))?.len();
    let Ok(Some(item_id)) = read_item_id(&file) else {
        return Err(FillError::Errno(libc::EIO));
    };
    let found = read_state(&file).ok().flatten();
    // §5.3 step 1: `state=hydrating`, `fsync`. The marker has to be durable
    // before the first byte lands, or a power loss leaves a file that looks
    // `online-only` while holding allocated blocks full of partial content,
    // and §4.4 startup recovery has nothing to find it by.
    write_state(&file, State::Hydrating).map_err(|e| FillError::Errno(errno_of(&e)))?;
    file.sync_all().map_err(|e| FillError::Errno(errno_of(&e)))?;
    if found != Some(State::OnlineOnly) {
        if let Some(clearance) = clearance {
            if let Err(e) = clearance.clear(&file).await {
                tracing::error!(
                    "{item_id}: the way was not cleared for filling a file found {found:?} \
                     ({e}); not filling it, since a failed fill would empty a file that may \
                     still be ignored"
                );
                put_back(&file, found);
                return Err(FillError::NotCleared(e));
            }
        }
    }

    fill(&file, &item_id, original_size, source).await.map_err(|errno| {
        roll_back(&file, original_size);
        FillError::Errno(errno)
    })
}

/// Undoes the `hydrating` a fill wrote before it had touched anything else.
fn put_back(file: &File, found: Option<State>) {
    let restored = match found {
        Some(state) => write_state(file, state),
        None => xattr::FileExt::remove_xattr(file, XATTR_STATE),
    };
    if let Err(e) = restored {
        tracing::error!(
            "cannot put the state back to {found:?} ({e}); the file is left `hydrating`, which \
             the next open fills again"
        );
    }
}

/// Undoes a failed fill: the file goes back to carrying its true size and no
/// content at all.
///
/// **Only a file still `hydrating` is touched** (Ruling H137). That is the
/// state this fill wrote, and under the per-inode lock nothing else in this
/// daemon changes it. The fill itself writes one more — `hydrated`, its
/// commit point — and only the final `fsync` can fail after it; by then the
/// content, its `fdatasync` and its stamp are all complete, so there is
/// nothing to undo, and the file is left as the correct, hydrated file it
/// is. Anything else means something outside the daemon changed the state,
/// and the file is no longer this fill's to empty — least of all one that
/// reads `hydrated`, which the helper lets every opener through and may have
/// ignore-marked. Emptying such a file is the zeros case. It is left exactly
/// as it is, and the failure is logged.
///
/// **The demotion comes first** (Ruling H48). The reverse order — punch,
/// resize, then demote, as §5.3 used to prescribe — has a window in which
/// the file holds no data while its state still says otherwise, and every
/// step here can fail on the same disk that just failed the fill. A crash or
/// a failed `write_state` inside this window leaves a `hydrated` file full of
/// zeros, which the helper then allows *and* ignore-marks: permanent, silent
/// data loss that looks like an empty file. Demoting first inverts that: the
/// worst outcome becomes an `online-only` file that still holds stale
/// content, which the next open simply overwrites.
///
/// The punch below is safe to make because of [`hydrate_with`]: the way was
/// cleared by Ruling H146's local rule once `hydrating` was durable, or the
/// file was `online-only`, and nothing places a mark on a file that reads
/// `hydrating`.
///
/// None of the four results is discarded. They are the only signal that this
/// window was ever entered.
fn roll_back(file: &File, original_size: u64) {
    match read_state(file) {
        Ok(Some(State::Hydrating)) => {}
        other => {
            tracing::error!(
                "a failed fill found its file {other:?}, not `hydrating`: either its own last \
                 fsync failed after the commit point, and the file is complete, or something \
                 outside the daemon changed the state; leaving it exactly as it is rather than \
                 empty a file that is no longer this fill's"
            );
            return;
        }
    }
    if let Err(e) = write_state(file, State::OnlineOnly) {
        tracing::error!("cannot demote a failed hydration back to online-only: {e}");
    }
    // §4.4 removes the stamp on recovery; a failed *re*-hydration would
    // otherwise leave the previous one's stamp on an online-only file.
    if let Err(e) = remove_stamp(file) {
        tracing::error!("cannot remove the stamp of a failed hydration: {e}");
    }
    if let Err(e) = punch_all(file) {
        tracing::error!("cannot punch away the partial content of a failed hydration: {e}");
    }
    if let Err(e) = file.set_len(original_size) {
        tracing::error!("cannot restore the size of a failed hydration: {e}");
    }
}

async fn fill(
    file: &File,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
) -> Result<(), i32> {
    // One buffer for the whole fill, not one per attempt.
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    let mut attempt = 0;
    let (size, mtime) = loop {
        match source.fetch(item_id, written).await {
            Ok(mut fetched) => {
                // Ruling H49: bytes are written where the source says they
                // start, or not at all. A source that answers a resume by
                // restarting the body at 0 would otherwise have the file's
                // beginning written over its middle, and the result reported
                // as a success.
                if fetched.served_from != written {
                    tracing::error!(
                        "{item_id}: asked for byte {written} and got a stream starting at {}; \
                         refusing rather than write it at the wrong offset",
                        fetched.served_from
                    );
                    return Err(libc::EIO);
                }
                // Ruling H50: a declared size of 0 against a placeholder that
                // carries a real size is refused, not obeyed. Obeying it
                // truncates live content on the strength of one unconfirmed
                // answer, with no retry — `written(0) >= size(0)` ends the
                // loop immediately. A genuinely emptied remote file is a
                // metadata change and belongs to the metadata sync path, not
                // to a hydration triggered by an open. Note the asymmetry is
                // deliberate: every *non-zero* resize, in either direction,
                // still goes through untouched below.
                if fetched.size == 0 && original_size != 0 {
                    tracing::error!(
                        "{item_id}: the source declares size 0 for a placeholder of \
                         {original_size} bytes; refusing to truncate it here"
                    );
                    return Err(libc::EIO);
                }
                loop {
                    let read = fetched
                        .stream
                        .read(&mut buffer)
                        .await
                        .map_err(|_| libc::EIO)?;
                    if read == 0 {
                        break;
                    }
                    // Positioned: `pwrite`, never `write`. The event fd is a
                    // descriptor the *application* is about to use, and it
                    // shares its file offset with the suspended `open()`.
                    file.write_all_at(&buffer[..read], written)
                        .map_err(|e| errno_of(&e))?;
                    written += read as u64;
                }
                if written >= fetched.size {
                    break (fetched.size, fetched.mtime);
                }
                // Short stream: retry from where we stopped.
                attempt += 1;
                if attempt >= 3 {
                    return Err(libc::EIO);
                }
            }
            // A missing remote item and a transient failure (a dropped
            // connection, a timeout, a 5xx) are both reported as EIO, never
            // as the errno the underlying cause might suggest (ENOENT,
            // ECONNRESET, ETIMEDOUT, ...): the kernel only accepts a fixed
            // small set of errnos on `FAN_DENY` (Ruling H16), and EIO is the
            // one in that set that fits "content could not be produced".
            Err(SourceError::NotFound(_)) => return Err(libc::EIO),
            Err(SourceError::Transient(_)) => {
                attempt += 1;
                if attempt >= 3 {
                    return Err(libc::EIO);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
            }
        }
    };

    // §5.3 steps 4 and 5, in the order Ruling H48 fixed: `state=hydrated` is
    // the **commit point** — it is what makes the helper allow the open and
    // ignore-mark the inode — so every step that can still fail runs before
    // it, never after it has landed. Written last: after the size is right,
    // after the data is durable, after the stamp. The old order wrote it
    // before `write_stamp` and `sync_all`, so a full or failing disk produced
    // a file marked `hydrated` holding nothing but zeros while this function
    // reported `EIO` — and the helper answers that by allowing, and
    // ignore-marking, the very next open.
    file.set_len(size).map_err(|e| errno_of(&e))?;
    // Ruling H61: an mtime the local filesystem cannot hold does **not** fail
    // the hydration. The property that outranks everything in this component
    // is "never serve zeros", and neither answer here serves zeros — the
    // file's content is complete and byte-for-byte correct either way — so
    // this is not a fail-closed decision at all. Denying would trade a
    // cosmetic metadata disagreement for the user not being able to read a
    // correct file, which is a real loss of availability for no safety gain.
    // An mtime we cannot apply is a metadata problem, and it belongs to the
    // metadata sync path, exactly where Ruling H50 put a contradictory *size*.
    //
    // The ordering still matters, and it is what keeps this safe: `write_stamp`
    // below records the size and mtime the file *actually has* at that moment,
    // so after a failed `futimens` the stamp holds the local mtime rather than
    // the source's. `stamp_matches` therefore still holds, and §8 dehydration
    // still works on this file instead of refusing it as "modified locally".
    if let Err(e) = set_mtime(file, mtime) {
        tracing::warn!(
            "{item_id}: cannot apply the mtime the source reported ({e}); hydrating anyway — the \
             content is complete and correct, and the stamp records the mtime the file actually \
             has, so dehydration still recognises it"
        );
    }
    file.sync_data().map_err(|e| errno_of(&e))?;
    write_stamp(file).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))?;
    // Test-only fault injection point (see `POST_DATA_FAULT` above): fires,
    // when armed, at the last instant before the commit write below and
    // nowhere else — a `#[cfg(test)]` no-op in every other build.
    #[cfg(test)]
    if let Some(errno) = fire_post_data_fault() {
        return Err(errno);
    }
    write_state(file, State::Hydrated).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))?;
    Ok(())
}

// Test-only fault injection for the one instant that matters most in
// `fill`'s tail: immediately before `state=hydrated` — the commit point —
// is written.
//
// Ruling H48 moved that write to be the *last* fallible step, so that any
// earlier failure leaves the file demoted, punched and stamp-less rather
// than `hydrated` over zeros. The only host-reachable failure downstream of
// the data landing used to be `set_mtime`'s (a pre-epoch mtime), and Ruling
// H61 correctly made that non-fatal — which took away the one test able to
// reach this window without a VM. This hook restores it: a test installs a
// closure, `fill` calls it right before the commit write and, if it returns
// an errno, bails out *without ever calling `write_state(Hydrated)`* — the
// same as any other post-data disk failure would.
//
// A thread-local, not a global: `#[tokio::test]` gives each test its own OS
// thread (and, by default, a single-threaded runtime pinned to it), so
// nothing here can leak between tests. Compiled out entirely outside
// `cfg(test)`, so it costs nothing in production.
#[cfg(test)]
type PostDataFaultHook = Box<dyn FnMut() -> Option<i32>>;

#[cfg(test)]
thread_local! {
    static POST_DATA_FAULT: std::cell::RefCell<Option<PostDataFaultHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_post_data_fault(hook: impl FnMut() -> Option<i32> + 'static) {
    POST_DATA_FAULT.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn clear_post_data_fault() {
    POST_DATA_FAULT.with(|cell| *cell.borrow_mut() = None);
}

#[cfg(test)]
fn fire_post_data_fault() -> Option<i32> {
    POST_DATA_FAULT.with(|cell| cell.borrow_mut().as_mut().and_then(|hook| hook()))
}

fn set_mtime(file: &File, mtime: SystemTime) -> io::Result<()> {
    use std::os::fd::AsFd;
    let since_epoch = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mtime before the epoch"))?;
    let spec = nix::sys::time::TimeSpec::new(
        since_epoch.as_secs() as i64,
        since_epoch.subsec_nanos() as i64,
    );
    nix::sys::stat::futimens(file.as_fd(), &spec, &spec)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek};
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;

    use konedrive_fs::placeholder::{read_state, read_stamp, State};
    use konedrive_proto::ACCEPTED_DENY_ERRNOS;

    use super::*;

    fn placeholder(dir: &std::path::Path, item_id: &str, size: u64) -> std::fs::File {
        let handle = std::fs::File::open(dir).unwrap();
        konedrive_fs::placeholder::create_placeholder(
            &handle,
            "file.bin",
            item_id,
            size,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
        )
        .unwrap();
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(dir.join("file.bin"))
            .unwrap()
    }

    /// Every errno this module produces travels to the kernel in a
    /// `FAN_DENY | (errno << 24)` response word, which the kernel accepts for
    /// exactly eight values; anything else makes that `write()` fail with
    /// `EINVAL` and leaves the suspended `open()` hanging forever. So every
    /// test that gets an errno out of `hydrate` puts it through here first —
    /// the property is not "this call returns EIO", it is "no call can ever
    /// return something undeliverable".
    fn deliverable(errno: i32) -> i32 {
        assert!(
            ACCEPTED_DENY_ERRNOS.contains(&errno),
            "errno {errno} is not one the kernel accepts in a FAN_DENY response; \
             the helper's write() would fail with EINVAL and the opener would hang forever"
        );
        errno
    }

    async fn hydrate_file(file: &std::fs::File, source: &dyn ContentSource) -> i32 {
        deliverable(hydrate(file.as_fd().try_clone_to_owned().unwrap(), source).await)
    }

    /// A source that never produces anything, to pin the retry-then-give-up
    /// path for `Transient` — which, unlike a short stream, nothing in the
    /// repo exercised.
    struct AlwaysTransient {
        attempts: AtomicU64,
    }

    #[async_trait]
    impl ContentSource for AlwaysTransient {
        async fn fetch(&self, _item_id: &str, _from: u64) -> Result<Fetched, SourceError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(SourceError::Transient("the connection dropped".into()))
        }
    }

    /// The HTTP `200`-instead-of-`206` shape, told honestly: the second fetch
    /// serves the file from the beginning again and says so.
    struct RestartsFromZero {
        path: PathBuf,
        break_at: u64,
        fetches: AtomicU64,
    }

    #[async_trait]
    impl ContentSource for RestartsFromZero {
        async fn fetch(&self, _item_id: &str, _from: u64) -> Result<Fetched, SourceError> {
            let n = self.fetches.fetch_add(1, Ordering::SeqCst);
            let meta = std::fs::metadata(&self.path).unwrap();
            let file = tokio::fs::File::open(&self.path).await.unwrap();
            let stream: Box<dyn AsyncRead + Send + Unpin> = if n == 0 {
                Box::new(file.take(self.break_at))
            } else {
                Box::new(file)
            };
            Ok(Fetched {
                served_from: 0,
                size: meta.len(),
                mtime: meta.modified().unwrap(),
                stream,
            })
        }
    }

    /// Serves a real file but declares a pre-epoch mtime — one `futimens`
    /// cannot be given. Everything about the *content* is correct; only the
    /// timestamp is unrepresentable here (Ruling H61).
    struct ImpossibleMtime {
        inner: LocalDir,
    }

    #[async_trait]
    impl ContentSource for ImpossibleMtime {
        async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            let mut fetched = self.inner.fetch(item_id, from).await?;
            fetched.mtime = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
            Ok(fetched)
        }
    }

    /// Reads the file's state from the filesystem at the moment the bytes are
    /// asked for, which is the only window in which `hydrating` exists.
    struct WatchesState {
        inner: LocalDir,
        path: PathBuf,
        seen: Mutex<Option<State>>,
    }

    #[async_trait]
    impl ContentSource for WatchesState {
        async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            let opened = std::fs::File::open(&self.path).unwrap();
            *self.seen.lock().unwrap() = read_state(&opened).unwrap();
            self.inner.fetch(item_id, from).await
        }
    }

    #[tokio::test]
    async fn fills_the_placeholder_in_place_and_marks_it_hydrated() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM1"), b"0123456789").unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM1", 10);

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, 0);

        let mut content = String::new();
        let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        opened.read_to_string(&mut content).unwrap();
        assert_eq!(content, "0123456789");
        assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
        assert!(konedrive_fs::placeholder::stamp_matches(&opened).unwrap());
    }

    /// I3: the placeholder is 4 KiB and the download grows it to 512 KiB
    /// before breaking, so both halves of the rollback are *visible*: the
    /// size has to come back down and the blocks have to go away. The
    /// original version of this test used a 4096-byte remote against a
    /// 4096-byte placeholder and asserted `blocks() < 64` — a bound no
    /// 4 KiB file can reach — so deleting either `punch_all` or the
    /// `set_len` left it green.
    #[tokio::test]
    async fn a_failed_download_leaves_an_empty_placeholder_and_an_errno() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM2"), vec![7u8; 1024 * 1024]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM2", 4096);

        let source = LocalDir::new(remote.path()).fail_at(512 * 1024);
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
        let meta = opened.metadata().unwrap();
        assert_eq!(meta.len(), 4096, "the placeholder's own size must come back");
        // Measured: 0 with the punch, 8 without it — the restored size alone
        // frees everything past the first 4 KiB, so any bound looser than
        // "no data at all" lets a missing `punch_all` through. On a realistic
        // placeholder those 8 blocks are however many the download managed.
        assert_eq!(
            meta.blocks(),
            0,
            "the 512 KiB that did arrive must be punched away, not merely truncated away"
        );
        assert_eq!(read_stamp(&opened).unwrap(), None, "a failed fill leaves no stamp");
    }

    #[tokio::test]
    async fn a_file_that_grew_remotely_is_resized() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM3"), b"much longer than before").unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM3", 4);

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, 0);

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(opened.metadata().unwrap().len(), 23);
    }

    /// C2: the growth direction resizes itself — `write_all_at` past the end
    /// extends the file whether or not anything calls `set_len`. Shrinking is
    /// the direction that needs the truncation, and it is the direction that
    /// loses data without it: a 1,000,000-byte placeholder marked `hydrated`
    /// while holding 4 real bytes and 999,996 zeros.
    #[tokio::test]
    async fn a_file_that_shrank_remotely_is_truncated() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM4"), b"tiny").unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM4", 1_000_000);

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, 0);

        let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(opened.metadata().unwrap().len(), 4, "the remote size wins");
        let mut content = Vec::new();
        opened.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"tiny");
        assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
        assert!(konedrive_fs::placeholder::stamp_matches(&opened).unwrap());
    }

    /// I4: the `NotFound` arm — a deleted remote item — was mapped to `EIO`
    /// (Ruling H16) with nothing exercising it. `ENOENT`, the errno it
    /// obviously "should" be, is outside the kernel's accepted set and would
    /// hang the opener forever.
    #[tokio::test]
    async fn a_missing_remote_item_is_refused_with_an_errno_the_kernel_accepts() {
        let remote = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "GONE", 4096);

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
        assert_eq!(opened.metadata().unwrap().len(), 4096);
    }

    /// I4: the `Transient` arm, the other one nothing in the repo reached.
    /// (The two backoffs really do sleep — 200 ms then 400 ms — rather than
    /// pulling `tokio`'s `test-util` feature into the whole workspace's
    /// dependency graph to fake them.)
    #[tokio::test]
    async fn a_source_that_keeps_failing_is_retried_three_times_then_refused() {
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM5", 4096);

        let source = AlwaysTransient { attempts: AtomicU64::new(0) };
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
        assert_eq!(source.attempts.load(Ordering::SeqCst), 3, "three attempts, then give up");

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
    }

    /// I4: a descriptor that is not a placeholder at all. The helper only
    /// ever sends managed files, but "the item id is unreadable" is a real
    /// disk-error path and it must not answer with something undeliverable.
    #[tokio::test]
    async fn a_file_with_no_item_id_is_refused_with_an_errno_the_kernel_accepts() {
        let remote = tempfile::tempdir().unwrap();
        let file = tempfile::tempfile().unwrap();

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
    }

    /// I8: a full disk must reach the application as `ENOSPC`, which §5.2
    /// step 5 and §9 both ask for by name and which the kernel does accept —
    /// flattening every local failure to `EIO` throws away the one thing the
    /// user can act on. Everything outside the accepted set still has to
    /// become `EIO`, because the alternative is an opener that never wakes.
    #[test]
    fn local_write_failures_keep_the_errnos_the_kernel_accepts() {
        assert_eq!(errno_of(&io::Error::from_raw_os_error(libc::ENOSPC)), libc::ENOSPC);
        assert_eq!(errno_of(&io::Error::from_raw_os_error(libc::EDQUOT)), libc::EDQUOT);
        for outside in [libc::EROFS, libc::EFBIG, libc::EBADF, libc::ENOENT] {
            assert_eq!(errno_of(&io::Error::from_raw_os_error(outside)), libc::EIO);
        }
        assert_eq!(errno_of(&io::Error::other("no errno at all")), libc::EIO);
        for errno in [libc::ENOSPC, libc::EDQUOT, libc::EROFS, libc::EFBIG] {
            deliverable(errno_of(&io::Error::from_raw_os_error(errno)));
        }
    }

    /// I7: §5.3 step 1. The marker is what §4.4 startup recovery finds a
    /// half-filled file by after a power loss; without it the blocks stay
    /// allocated forever. It exists only while the bytes are in flight, so
    /// the source is where it can be observed.
    #[tokio::test]
    async fn the_file_is_marked_hydrating_while_the_bytes_are_in_flight() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM6"), vec![3u8; 8192]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM6", 8192);

        let source = WatchesState {
            inner: LocalDir::new(remote.path()),
            path: local.path().join("file.bin"),
            seen: Mutex::new(None),
        };
        assert_eq!(hydrate_file(&file, &source).await, 0);

        assert_eq!(
            *source.seen.lock().unwrap(),
            Some(State::Hydrating),
            "the file must be marked hydrating before the first byte is asked for"
        );
    }

    /// I5 (Ruling H49), the positive half: a file completed across two
    /// fetches. `fail_at` is permanent, so before this nothing followed the
    /// resume arithmetic end to end — and the resume offset is exactly where
    /// bytes land in the wrong place.
    #[tokio::test]
    async fn a_download_that_resumes_completes_the_file_byte_for_byte() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM7"), &payload).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM7", 3000);

        let source = LocalDir::new(remote.path()).fail_once_at(1000);
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.fetches(), 2, "the first fetch must have been resumed, not restarted");

        let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        let mut content = Vec::new();
        opened.read_to_end(&mut content).unwrap();
        assert_eq!(content, payload, "every byte must be where the source put it");
        assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
    }

    /// I5 (Ruling H49), the negative half: the source answers the resume with
    /// the whole file again — an HTTP server replying `200` to a `Range`
    /// request — and says so. Writing that at the resume offset produces a
    /// file whose middle is its beginning, reported as a success.
    #[tokio::test]
    async fn a_source_that_restarts_the_stream_is_refused_instead_of_written_at_the_wrong_offset() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM8"), &payload).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM8", 3000);

        let source = RestartsFromZero {
            path: remote.path().join("ITEM8"),
            break_at: 1000,
            fetches: AtomicU64::new(0),
        };
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(
            read_state(&opened).unwrap(),
            Some(State::OnlineOnly),
            "a file filled from a stream at the wrong offset must never be marked hydrated"
        );
        assert_eq!(opened.metadata().unwrap().len(), 3000);
        // Not `blocks() == 0` here: a 3000-byte file's punch range ends
        // mid-page, and no filesystem can release a page it has only
        // partially punched — measured, 8 blocks survive on tmpfs. What must
        // hold either way is that none of the bytes that did arrive are
        // still readable.
        let mut content = Vec::new();
        std::fs::File::open(local.path().join("file.bin"))
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert!(
            content.iter().all(|b| *b == 0),
            "the partial content must have been punched away, not left in place"
        );
    }

    /// I6 (Ruling H50): `written(0) >= size(0)` ends the fill loop on the
    /// first answer, so a declared size of 0 truncates a live placeholder
    /// with no retry and no corroboration whatsoever. Fail closed instead.
    #[tokio::test]
    async fn a_declared_size_of_zero_does_not_truncate_a_live_placeholder() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM9"), b"").unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM9", 1_000_000);

        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);

        let opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        assert_eq!(opened.metadata().unwrap().len(), 1_000_000, "the placeholder survives");
        assert_eq!(read_state(&opened).unwrap(), Some(State::OnlineOnly));
    }

    /// Ruling H61. An mtime the local filesystem cannot represent must not
    /// cost the user a file whose **content is entirely correct**.
    ///
    /// The governing property of this whole sub-project is "never serve
    /// zeros", and neither answer here serves zeros: the bytes are all
    /// present either way. So denying is not the fail-closed choice, it is
    /// simply a refusal to hand over correct content because of a timestamp
    /// — availability spent for no safety at all. (The previous round made
    /// this fatal, which went beyond its brief.)
    ///
    /// The second half is what keeps it safe rather than merely lenient: the
    /// stamp has to record the mtime the file *actually ends up with*, not
    /// the one that could not be applied, or `stamp_matches` would be false
    /// from birth and spec §8 would refuse to dehydrate the file ever again,
    /// reporting it as "modified locally".
    #[tokio::test]
    async fn an_mtime_the_filesystem_cannot_hold_still_hydrates_the_file() {
        let remote = tempfile::tempdir().unwrap();
        let payload = vec![5u8; 200_000];
        std::fs::write(remote.path().join("ITEM11"), &payload).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM11", 4096);

        let source = ImpossibleMtime { inner: LocalDir::new(remote.path()) };
        assert_eq!(hydrate_file(&file, &source).await, 0, "correct content must not be denied");

        let mut opened = std::fs::File::open(local.path().join("file.bin")).unwrap();
        let mut content = Vec::new();
        opened.read_to_end(&mut content).unwrap();
        assert_eq!(content, payload, "every byte of the file is there");
        assert_eq!(read_state(&opened).unwrap(), Some(State::Hydrated));
        assert!(
            konedrive_fs::placeholder::stamp_matches(&opened).unwrap(),
            "the stamp must record the mtime the file actually has, or §8 would refuse to \
             dehydrate this file for the rest of its life"
        );
        // And the stamp is a real one, not the unrepresentable value: the file
        // kept a local, post-epoch mtime, which is exactly what makes the
        // stamp agree with it.
        let mtime = opened.metadata().unwrap().modified().unwrap();
        assert!(
            mtime > SystemTime::UNIX_EPOCH,
            "the pre-epoch mtime was never applied; the file keeps a representable one"
        );
    }

    /// Verified correct by the review and pinned here so it stays that way:
    /// the fill is positioned (`pwrite`), so the file offset the suspended
    /// `open()` is about to inherit is exactly where the application left it.
    /// A plain `write` would move it by the size of the whole download.
    #[tokio::test]
    async fn the_shared_file_offset_is_untouched_by_a_fill() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM10"), vec![9u8; 100_000]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let mut file = placeholder(local.path(), "ITEM10", 100_000);
        file.seek(io::SeekFrom::Start(1234)).unwrap();

        // The duplicate shares one open file description with `file`, exactly
        // as the helper's `SCM_RIGHTS` copy shares one with the opener's.
        let source = LocalDir::new(remote.path());
        assert_eq!(hydrate_file(&file, &source).await, 0);

        assert_eq!(file.stream_position().unwrap(), 1234);
    }

    /// C1 (Ruling H48), pinned back onto the host suite. This is the test
    /// `an_mtime_the_filesystem_cannot_hold_still_hydrates_the_file` used to
    /// be, before Ruling H61 correctly made `set_mtime`'s error non-fatal and
    /// took away the only post-data failure an unprivileged test could reach.
    ///
    /// The injected fault fires unconditionally, right before the commit
    /// write, and — from *inside* the fault itself — re-opens the placeholder
    /// and asserts it is not yet observably `hydrated`. That is what actually
    /// pins the ordering: a fault that merely makes some fallible call return
    /// an error cannot distinguish "commit point last" from "commit point
    /// early, but this particular later step happened to fail too", because
    /// `hydrate`'s rollback demotes the file either way once `fill` returns
    /// `Err`. Observing the state *at the moment of the fault*, before
    /// `fill` has had any chance to return, is what a moved-up commit point
    /// cannot survive.
    #[tokio::test]
    async fn a_post_data_failure_never_leaves_the_file_observably_hydrated() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM12"), vec![4u8; 65536]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = placeholder(local.path(), "ITEM12", 65536);
        let path = local.path().join("file.bin");

        let hook_path = path.clone();
        set_post_data_fault(move || {
            let opened = std::fs::File::open(&hook_path).unwrap();
            assert_ne!(
                read_state(&opened).unwrap(),
                Some(State::Hydrated),
                "state=hydrated must be the LAST fallible step in fill's tail; this fault \
                 fires before the commit write and must never observe it already landed"
            );
            Some(libc::EIO)
        });

        let source = LocalDir::new(remote.path());
        let errno = hydrate_file(&file, &source).await;
        clear_post_data_fault();

        assert_eq!(errno, libc::EIO);
        let opened = std::fs::File::open(&path).unwrap();
        assert_eq!(
            read_state(&opened).unwrap(),
            Some(State::OnlineOnly),
            "a post-data failure must leave the file demoted, never hydrated"
        );
        assert_eq!(opened.metadata().unwrap().len(), 65536, "the placeholder's size must come back");
        assert_eq!(
            opened.metadata().unwrap().blocks(),
            0,
            "the data that landed before the fault must be punched away"
        );
        assert_eq!(read_stamp(&opened).unwrap(), None, "a failed fill leaves no stamp");
    }

    /// Ruling H137: a roll-back never punches a file that is no longer in the
    /// state its fill put it in. Under the per-inode lock only something
    /// outside the daemon can have changed it — and whatever did, the file is
    /// not this fill's to empty any more: a file that reads `hydrated` may be
    /// carrying an ignore mark, and punching it is the zeros case.
    #[test]
    fn a_roll_back_leaves_alone_a_file_that_is_no_longer_hydrating() {
        let dir = tempfile::tempdir().unwrap();
        let file = placeholder(dir.path(), "ITEM", 4096);
        std::fs::write(dir.path().join("file.bin"), vec![6u8; 4096]).unwrap();
        write_state(&file, State::Hydrated).unwrap();

        roll_back(&file, 4096);

        let mut content = Vec::new();
        std::fs::File::open(dir.path().join("file.bin")).unwrap().read_to_end(&mut content).unwrap();
        assert!(content == vec![6u8; 4096], "the file's content must be left alone");
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated), "and so must its state");
    }
}
