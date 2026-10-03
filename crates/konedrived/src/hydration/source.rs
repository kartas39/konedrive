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
    punch_all, punch_from, read_item_id, read_progress, read_state, remove_progress, remove_stamp,
    stamp_matches, write_ctag, write_progress, write_stamp, write_state, Progress, State, XATTR_PROGRESS,
    XATTR_STATE,
};
use konedrive_proto::clamp_deny_errno;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::helper::{Clearance, HelperLink, NotCleared};
use konedrive_graph::quickxor::QuickXor;

pub mod parts;

pub use parts::{Share, Split};

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
    /// way to notice after the fact. This is not a theoretical
    /// implementor: the Graph source issues HTTP `Range` requests, and a
    /// server that answers `200` instead of `206` — which is always allowed —
    /// restarts the body at 0. Reporting the served offset is the only thing
    /// that makes that case detectable, so every implementation must set it
    /// to the offset of the first byte of `stream`, not to the offset it was
    /// asked for.
    pub served_from: u64,
    pub size: u64,
    pub mtime: SystemTime,
    /// `None` from a source that cannot say (`LocalDir`): nothing is then
    /// verified or checkpointed, exactly as in part 1.
    pub version: Option<Version>,
    pub stream: Box<dyn AsyncRead + Send + Unpin>,
}

/// Which version of a file a source's bytes belong to, when it can say: the
/// cTag, and the quickXorHash the whole file must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub ctag: String,
    pub quick_xor: Option<[u8; konedrive_graph::quickxor::LEN]>,
}

#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Bytes of `item_id` starting at `from`, plus the item's current size and mtime.
    ///
    /// `end` is where the bytes wanted stop — the offset of the first byte not
    /// wanted — or `None` for the rest of the file: one piece of a download
    /// in parts (issue #28) asks for its own range only. A source may still
    /// serve more than that; the download reads no further than `end`.
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError>;

    /// How far a download in parts has come as a whole: `done` bytes of the
    /// file's `size` are on disk. No one of its streams can tell, so the
    /// download says it here; a source that shows progress
    /// (`tracked::Tracked`) shows this one.
    fn progress(&self, _done: u64, _size: u64) {}
}

/// Why a source file must not be read into a placeholder, or
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
pub(crate) fn refused_source_path(path: &Path, root: &Path) -> io::Result<Option<String>> {
    let resolved = std::fs::canonicalize(path)?;
    let names = xattr::list_deref(path)?;
    Ok(refused_source(names, &resolved, root))
}

/// Test source: one file per item id in a directory, with fault injection.
pub struct LocalDir {
    dir: PathBuf,
    /// The sync folder this source fills, whose own files it refuses to be
    /// read from; `None` for a source that fills nothing real.
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
    /// this source fills, or that is one of konedrive's own files —
    /// decided on the descriptor the bytes would be read from, so
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
    /// later fetch serves the rest of the file.
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
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let fetch_number = self.fetches.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let path = self.dir.join(item_id);
        // One open, and everything decided on it: what it is, whether it may
        // be read, and the bytes.
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
        if let Some(end) = end {
            stream = Box::new(stream.take(end.saturating_sub(from)));
        }
        Ok(Fetched {
            served_from: from,
            size: meta.len(),
            mtime: meta.modified().map_err(|e| SourceError::Transient(e.to_string()))?,
            version: None,
            stream,
        })
    }
}

/// Maps a local filesystem failure onto the errno the suspended `open()` is
/// answered with.
///
/// A **clamp**, not a flattening: `ENOSPC` and `EDQUOT` are in
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
    /// It never started: the file may carry an ignore mark
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
/// per-inode lock: **look again**, and fill only a
/// file that still needs it.
///
/// A request can wait a long time before it gets here — for one of the four
/// fill slots behind other downloads, or in the helper for credit — and the
/// file it names can have been filled meanwhile: by `Hydrate()`, or because a
/// `Dehydrate` that the open itself made fail (its suspended descriptor
/// refuses the lease) rolled the file back to `hydrated`. Filling it again
/// without looking was The re-fill wrote `hydrating`
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
) -> Answered {
    let file = File::from(fd);
    match read_state(&file) {
        Ok(Some(State::Hydrated)) => {
            let edited = !matches!(stamp_matches(&file), Ok(true));
            tracing::info!(
                "a hydration request found its file already hydrated — filled while the request \
                 waited{} — and answers it without touching the file",
                if edited { ", and changed since (or never stamped): its content is kept" } else { "" }
            );
            Answered::AlreadyThere
        }
        Ok(Some(_)) => {
            let clearance = link.map(|link| Clearance::Link(link.clone()));
            match fill_file(file, source, clearance.as_ref(), None).await {
                Ok(()) => Answered::Filled,
                Err(e) => Answered::Failed(e),
            }
        }
        Ok(None) => {
            tracing::error!(
                "a hydration request names a file with no konedrive state; it is not ours to fill"
            );
            Answered::NotOurs
        }
        Err(e) => {
            tracing::error!("cannot read the state of a file a hydration request names: {e}");
            Answered::NotOurs
        }
    }
}

/// What [`answer_request`] did, which decides both what the opener is
/// answered ([`errno`](Self::errno)) and what the activity log records
///: only a fill that ran is an event.
#[derive(Debug)]
pub enum Answered {
    /// Filled while the request waited, or edited here: left as it is.
    AlreadyThere,
    Filled,
    /// The fill ran, or could not start, and the file is as it was.
    Failed(FillError),
    /// No konedrive state, or none that can be read: not ours to fill.
    NotOurs,
}

impl Answered {
    /// What the suspended open is answered with; 0 lets it through.
    pub fn errno(&self) -> i32 {
        match self {
            Answered::AlreadyThere | Answered::Filled => 0,
            Answered::Failed(e) => e.errno(),
            Answered::NotOurs => libc::EIO,
        }
    }
}

/// [`hydrate`], clearing the file's ignore mark first when it could be
/// carrying one.
///
/// # Can this file carry a mark placed after the last clear?
///
/// That is the question every punch has to answer, and a fill can punch:
/// [`roll_back`] empties the file
/// when the fill fails. The helper places an ignore mark only on a file it
/// has read `hydrated` — and reads again, after placing it —
/// so which state the fill starts from decides the answer:
///
/// - `online-only`: no. An open of it raised an event, so it carried no
///   mark then, and none can be placed once this fill has written
///   `hydrating`. (A stale mark on an `online-only` file already reads
///   zeros; filling the file repairs that, and failing leaves it as it was.)
/// - `hydrated` (only `Hydrate()` fills one, when its stamp is missing, H109)
///   and `dehydrating` (a `Dehydrate` cancelled between its state write and
/// its `ClearIgnore`): yes. `hydrating`, which a panicked or
///   crashed fill of such a file leaves: possibly.
///
/// For those, `hydrating` is made durable first and then the way is cleared
/// by local rule ([`Clearance`]) — the helper asked to
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
    fill_file(File::from(fd), source, clearance, None).await
}

/// [`hydrate_with`], downloading the file in parallel parts ([`parts`]): a large pinned
/// file (issue #28). Everything else about the fill — the state, the clearance, the
/// checkpoint, the roll-back and the commit — is the same.
pub async fn hydrate_in_parts(
    fd: OwnedFd,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
    split: &Split,
) -> Result<(), FillError> {
    fill_file(File::from(fd), source, clearance, Some(split)).await
}

async fn fill_file(
    file: File,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
    split: Option<&Split>,
) -> Result<(), FillError> {
    let meta = file.metadata().map_err(|e| FillError::Errno(errno_of(&e)))?;
    // The placeholder's own time — the cloud's — which a roll-back puts back
    // over what the fill's writes made of it (A-I1).
    let original = (meta.len(), meta.modified().ok());
    let original_size = original.0;
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

    fill(&file, &item_id, original_size, source, split).await.map_err(|errno| {
        roll_back(&file, original_size, original.1);
        FillError::Errno(errno)
    })
}

/// A file whose download was stopped part-way because its item was removed
/// (issue #104), and which survives — set aside for another account, or
/// left by a removal that failed: a placeholder again, with no content and
/// no checkpoint, never a partly filled file. Only a file still `hydrating`
/// is touched; the caller holds its inode lock.
pub(crate) fn back_to_placeholder(file: &File) {
    if !matches!(read_state(file), Ok(Some(State::Hydrating))) {
        return;
    }
    // Punching needs a descriptor open for writing.
    let writable = match konedrive_fs::placeholder::reopen_writable(file) {
        Ok(writable) => writable,
        Err(e) => {
            tracing::error!("cannot reopen a stopped download to turn it back into a placeholder: {e}");
            return;
        }
    };
    let file = &writable;
    if let Err(e) = write_state(file, State::OnlineOnly) {
        tracing::error!("cannot turn a stopped download back into a placeholder: {e}");
        return;
    }
    if let Err(e) = remove_stamp(file) {
        tracing::error!("cannot remove the stamp of a stopped download: {e}");
    }
    if let Err(e) = remove_progress(file) {
        tracing::error!("cannot remove the checkpoint of a stopped download: {e}");
    }
    if let Err(e) = punch_all(file) {
        tracing::error!("cannot punch away the partial content of a stopped download: {e}");
    }
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
/// content at all — or, when the download made a checkpoint durable,
/// only the checkpointed prefix and its `user.konedrive.progress`, which
/// the next fill continues from.
///
/// **Only a file still `hydrating` is touched**. That is the
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
/// **The demotion comes first**. The reverse order — punch,
/// resize, then demote, as §5.3 used to prescribe — has a window in which
/// the file holds no data while its state still says otherwise, and every
/// step here can fail on the same disk that just failed the fill. A crash or
/// a failed `write_state` inside this window leaves a `hydrated` file full of
/// zeros, which the helper then allows *and* ignore-marks: permanent, silent
/// data loss that looks like an empty file. Demoting first inverts that: the
/// worst outcome becomes an `online-only` file that still holds stale
/// content, which the next open simply overwrites.
///
/// What follows the demotion keeps the checkpointed prefix when there is one
///: only what lies past it is punched. That prefix is not trusted
/// by being kept — the fill that continues from it reads it back into the
/// hash, and the whole file, prefix included, is checked against its
/// quickXorHash before it is ever `hydrated`. With no usable checkpoint, the
/// checkpoint attribute goes first and then every block, as before.
///
/// The punch below is safe to make because of [`hydrate_with`]: the way was
/// cleared by local rule once `hydrating` was durable, or the
/// file was `online-only`, and nothing places a mark on a file that reads
/// `hydrating`.
///
/// The file gets back the time it had before the fill (`original_mtime`, the
/// cloud's for a placeholder): the fill's writes moved it to now, and a
/// placeholder whose time is not the cloud's has its thumbnail refused by
/// KIO — which then opens the file to make one of its own, a download
/// (A-I1).
///
/// None of the results is discarded. They are the only signal that this
/// window was ever entered.
fn roll_back(file: &File, original_size: u64, original_mtime: Option<SystemTime>) {
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
    // A checkpoint the download made durable is kept, with the
    // bytes it counts; the next fill continues from it.
    let keep = usable_checkpoint(file, original_size);
    if let Err(e) = write_state(file, State::OnlineOnly) {
        tracing::error!("cannot demote a failed hydration back to online-only: {e}");
    }
    // §4.4 removes the stamp on recovery; a failed *re*-hydration would
    // otherwise leave the previous one's stamp on an online-only file.
    if let Err(e) = remove_stamp(file) {
        tracing::error!("cannot remove the stamp of a failed hydration: {e}");
    }
    match &keep {
        Some(progress) => {
            if let Err(e) = punch_from(file, progress.bytes) {
                tracing::error!("cannot punch away the part of a failed hydration past its checkpoint: {e}");
            }
            tracing::info!("a failed hydration keeps its first {} bytes for the next attempt", progress.bytes);
        }
        None => {
            if let Err(e) = remove_progress(file) {
                tracing::error!("cannot remove the checkpoint of a failed hydration: {e}");
            }
            if let Err(e) = punch_all(file) {
                tracing::error!("cannot punch away the partial content of a failed hydration: {e}");
            }
        }
    }
    if let Err(e) = file.set_len(original_size) {
        tracing::error!("cannot restore the size of a failed hydration: {e}");
    }
    if let Some(mtime) = original_mtime {
        if let Err(e) = set_mtime(file, mtime) {
            tracing::error!("cannot restore the time of a failed hydration: {e}");
        }
    }
}

/// How often a fill makes its progress durable.
pub const CHECKPOINT_EVERY: u64 = 16 * 1024 * 1024;

// A test's own checkpoint interval, so that a checkpoint can be reached with
// a file of a few hundred KiB. A thread-local for the same reason as
// `POST_DATA_FAULT` below: `#[tokio::test]` runs each test on its own thread.
#[cfg(test)]
thread_local! {
    static CHECKPOINT_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn set_checkpoint_every(bytes: u64) {
    CHECKPOINT_OVERRIDE.with(|cell| cell.set(Some(bytes)));
}

#[cfg(test)]
fn clear_checkpoint_every() {
    CHECKPOINT_OVERRIDE.with(|cell| cell.set(None));
}

fn checkpoint_every() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = CHECKPOINT_OVERRIDE.with(|cell| cell.get()) {
        return bytes;
    }
    CHECKPOINT_EVERY
}

/// What a download produced, before any of it is committed.
pub(crate) struct Downloaded {
    pub size: u64,
    pub mtime: SystemTime,
    pub version: Option<Version>,
}

async fn fill(
    file: &File,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    split: Option<&Split>,
) -> Result<(), i32> {
    let resume = usable_checkpoint(file, original_size);
    if resume.is_none() {
        drop_unusable_checkpoint(file)?;
    }
    let downloaded = match split {
        Some(split) => parts::download(file, item_id, original_size, source, resume, split).await?,
        None => download(file, item_id, original_size, source, resume, true).await?,
    };
    commit(file, &downloaded)
}

/// Removes a `user.konedrive.progress` that [`usable_checkpoint`] will not
/// continue from — empty, past the end of the file, unreadable — before a
/// single new byte is written under it, and makes the removal durable. Left
/// in place, it would count bytes it knows nothing about, and after a crash
/// recovery's own test (the count against the file's size *by then*, which
/// the download may have grown) could keep them as a checkpoint.
///
/// Only when there is one: a removal lifts the lock's write bit for a moment
/// (`with_owner_write`), which a file with no checkpoint gives no reason for.
fn drop_unusable_checkpoint(file: &File) -> Result<(), i32> {
    if let Ok(None) = xattr::FileExt::get_xattr(file, XATTR_PROGRESS) {
        return Ok(());
    }
    tracing::info!("dropping a download checkpoint that cannot be continued from");
    remove_progress(file).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))
}

/// A download into a file nobody else can see — replacement of a
/// changed file: no checkpoints (an `O_TMPFILE` does not survive a crash) and
/// no size guard (there is no placeholder whose size it could contradict).
#[allow(dead_code)] // replacements are its first caller.
pub(crate) async fn download_into(file: &File, item_id: &str, source: &dyn ContentSource) -> Result<Downloaded, i32> {
    download(file, item_id, 0, source, None, false).await
}

/// A checkpoint an earlier download left, if it can be continued from: well
/// formed, and not past the end of the file (a file cut shorter since cannot
/// still hold the bytes it counts).
fn usable_checkpoint(file: &File, size: u64) -> Option<Progress> {
    match read_progress(file) {
        Ok(Some(progress)) if progress.bytes > 0 && progress.bytes <= size => Some(progress),
        _ => None,
    }
}

fn same_version(a: &Option<Version>, b: &Option<Version>) -> bool {
    a.as_ref().map(|v| &v.ctag) == b.as_ref().map(|v| &v.ctag)
}

/// The hash of the first `bytes` of `file`, read back from disk — the state a
/// resumed download continues from. `None` if they cannot all be read.
fn rehash(file: &File, bytes: u64, buffer: &mut [u8]) -> Option<QuickXor> {
    let mut hasher = QuickXor::new();
    let mut at = 0u64;
    while at < bytes {
        let want = buffer.len().min((bytes - at) as usize);
        let read = file.read_at(&mut buffer[..want], at).ok()?;
        if read == 0 {
            return None;
        }
        hasher.update(&buffer[..read]);
        at += read as u64;
    }
    Some(hasher)
}

/// Streams the file's bytes into `file` and checks them. Returns what to
/// commit, or the errno to answer with — every value in
/// `ACCEPTED_DENY_ERRNOS`.
///
/// - The version of the first answer is the one the file must end up as. A
///   later answer for another version means the file changed in the cloud
///   mid-download: the download starts over, once.
/// - A checkpoint is continued only for the same version, and only when that
///   version has a quickXorHash, after its bytes are read back into the hash;
///   anything else starts from zero. Without a hash nothing could tell a
/// damaged prefix from a good one, so a version without one is
///   never checkpointed either, and a failed download of it keeps nothing.
/// - A read error or a short stream is a break: the next fetch asks from where
///   the bytes stopped. Three breaks and the download gives up.
/// - A hash that does not match starts the download over, once; a second
///   mismatch is `EIO`, and drops whatever checkpoint the second download
///   made. Nothing unverified is ever committed when the source gave a hash.
///
/// A missing remote item and a transient failure (a dropped connection, a
/// timeout, a 5xx) are both reported as `EIO`, never as the errno the
/// underlying cause might suggest (`ENOENT`, `ECONNRESET`, `ETIMEDOUT`, ...):
/// the kernel only accepts a fixed small set of errnos on `FAN_DENY`,
/// and `EIO` is the one in that set that fits "content could not be
/// produced".
async fn download(
    file: &File,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    mut resume: Option<Progress>,
    checkpoints: bool,
) -> Result<Downloaded, i32> {
    // One buffer for the whole download, not one per attempt.
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = resume.as_ref().map_or(0, |p| p.bytes);
    let mut last_checkpoint = written;
    let mut hasher = QuickXor::new();
    // The version the bytes on disk belong to, once the first answer says.
    let mut expected: Option<Option<Version>> = None;
    let mut breaks = 0u32;
    let mut started_over = false;

    // Drops everything and starts from byte 0 on the next fetch.
    macro_rules! start_over {
        () => {{
            written = 0;
            last_checkpoint = 0;
            hasher = QuickXor::new();
            expected = None;
            resume = None;
            if checkpoints {
                remove_progress(file).map_err(|e| errno_of(&e))?;
            }
        }};
    }

    loop {
        let fetched = match source.fetch(item_id, written, None).await {
            Ok(fetched) => fetched,
            Err(SourceError::NotFound(_)) => return Err(libc::EIO),
            Err(SourceError::Transient(why)) => {
                breaks += 1;
                if breaks >= 3 {
                    tracing::warn!("{item_id}: giving up after three failures: {why}");
                    return Err(libc::EIO);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200 * breaks as u64)).await;
                continue;
            }
        };
        // Bytes are written where the source says they start, or
        // not at all. A source that answers a resume by restarting the body
        // at 0 would otherwise have the file's beginning written over its
        // middle, and the result reported as a success.
        if fetched.served_from != written {
            tracing::error!(
                "{item_id}: asked for byte {written} and got a stream starting at {}; refusing \
                 rather than write it at the wrong offset",
                fetched.served_from
            );
            return Err(libc::EIO);
        }
        // A declared size of 0 against a placeholder that carries
        // a real size is refused, not obeyed. Obeying it truncates live
        // content on the strength of one unconfirmed answer, with no retry —
        // `written(0) >= size(0)` ends the download immediately. A genuinely
        // emptied remote file is a metadata change and belongs to the
        // metadata sync path, not to a hydration triggered by an open. The
        // asymmetry is deliberate: every *non-zero* resize, in either
        // direction, still goes through untouched.
        if fetched.size == 0 && original_size != 0 {
            tracing::error!(
                "{item_id}: the source declares size 0 for a placeholder of {original_size} bytes; \
                 refusing to truncate it here"
            );
            return Err(libc::EIO);
        }
        match &expected {
            None => {
                if let Some(progress) = &resume {
                    // The same version, and one with a hash that will check the
                    // prefix along with the rest: nothing else vouches for bytes
                    // that lay on disk across a failure or a crash.
                    let same = fetched
                        .version
                        .as_ref()
                        .is_some_and(|v| v.ctag == progress.ctag && v.quick_xor.is_some());
                    match same.then(|| rehash(file, progress.bytes, &mut buffer)).flatten() {
                        Some(rebuilt) => hasher = rebuilt,
                        None => {
                            tracing::info!(
                                "{item_id}: the checkpoint at byte {} is for another version, has \
                                 no hash to be checked against, or cannot be read back; \
                                 downloading from the start",
                                progress.bytes
                            );
                            start_over!();
                            continue;
                        }
                    }
                }
                expected = Some(fetched.version.clone());
            }
            Some(version) if !same_version(version, &fetched.version) => {
                if started_over {
                    tracing::error!("{item_id}: the file keeps changing in the cloud while it downloads");
                    return Err(libc::EIO);
                }
                tracing::info!("{item_id}: the file changed in the cloud mid-download; starting over");
                started_over = true;
                start_over!();
                continue;
            }
            Some(_) => {}
        }
        let version = expected.clone().flatten();
        let mut stream = fetched.stream;
        // A read error mid-stream is a dropped connection, and is
        // resumed like a short stream, not answered `EIO` on the spot.
        let broke = loop {
            let read = match stream.read(&mut buffer).await {
                Ok(read) => read,
                Err(e) => {
                    tracing::warn!("{item_id}: the download broke after {written} bytes: {e}");
                    break true;
                }
            };
            if read == 0 {
                break false;
            }
            // Positioned: `pwrite`, never `write`. The event fd is a
            // descriptor the *application* is about to use, and it shares its
            // file offset with the suspended `open()`.
            file.write_all_at(&buffer[..read], written).map_err(|e| errno_of(&e))?;
            hasher.update(&buffer[..read]);
            written += read as u64;
            // The bytes first, durably, then the count that says
            // they are there — never a count ahead of the data it vouches for.
            // Only for a version with a hash: no resume would trust any other.
            if checkpoints && written - last_checkpoint >= checkpoint_every() {
                if let Some(Version { ctag, quick_xor: Some(_) }) = &version {
                    file.sync_data().map_err(|e| errno_of(&e))?;
                    write_progress(file, &Progress { ctag: ctag.clone(), bytes: written })
                        .map_err(|e| errno_of(&e))?;
                    last_checkpoint = written;
                }
            }
        };
        if !broke && written >= fetched.size {
            match version.as_ref().map(|v| v.quick_xor) {
                Some(Some(want)) if hasher.finish() != want || written != fetched.size => {
                    if started_over {
                        tracing::error!("{item_id}: the content does not match its quickXorHash, twice");
                        // Its checkpoints count bytes of content that failed
                        // the hash: nothing to continue from.
                        if checkpoints {
                            remove_progress(file).map_err(|e| errno_of(&e))?;
                        }
                        return Err(libc::EIO);
                    }
                    tracing::warn!("{item_id}: the content does not match its quickXorHash; downloading it once more");
                    started_over = true;
                    start_over!();
                    continue;
                }
                Some(None) => {
                    //
                    tracing::warn!("{item_id}: OneDrive gave no quickXorHash; the content could not be verified");
                }
                _ => {}
            }
            return Ok(Downloaded { size: fetched.size, mtime: fetched.mtime, version });
        }
        // A break, or a short stream: continue from where the bytes stopped.
        breaks += 1;
        if breaks >= 3 {
            return Err(libc::EIO);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200 * breaks as u64)).await;
    }
}

/// Steps 4 and 5, in order: `state=hydrated`, the
/// commit point, is written last, after the size, the data, the cTag and the
/// stamp are durable.
///
/// `state=hydrated` is what makes the helper allow the open and ignore-mark
/// the inode, so every step that can still fail runs before it, never after
/// it has landed. The old order wrote it before `write_stamp` and `sync_all`,
/// so a full or failing disk produced a file marked `hydrated` holding
/// nothing but zeros while the fill reported `EIO` — and the helper answers
/// that by allowing, and ignore-marking, the very next open.
fn commit(file: &File, downloaded: &Downloaded) -> Result<(), i32> {
    file.set_len(downloaded.size).map_err(|e| errno_of(&e))?;
    // An mtime the local filesystem cannot hold does **not** fail
    // the hydration. The property that outranks everything in this component
    // is "never serve zeros", and neither answer here serves zeros — the
    // file's content is complete and byte-for-byte correct either way — so
    // this is not a fail-closed decision at all. Denying would trade a
    // cosmetic metadata disagreement for the user not being able to read a
    // correct file. An mtime we cannot apply is a metadata problem, and it
    // belongs to the metadata sync path, exactly where put a
    // contradictory *size*.
    //
    // The ordering still matters, and it is what keeps this safe: `write_stamp`
    // below records the size and mtime the file *actually has* at that moment,
    // so after a failed `futimens` the stamp holds the local mtime rather than
    // the source's. `stamp_matches` therefore still holds, and §8 dehydration
    // still works on this file instead of refusing it as "modified locally".
    if let Err(e) = set_mtime(file, downloaded.mtime) {
        tracing::warn!(
            "cannot apply the mtime the source reported ({e}); hydrating anyway — the content is \
             complete and correct, and the stamp records the mtime the file actually has"
        );
    }
    file.sync_data().map_err(|e| errno_of(&e))?;
    if let Some(version) = &downloaded.version {
        write_ctag(file, &version.ctag).map_err(|e| errno_of(&e))?;
    }
    remove_progress(file).map_err(|e| errno_of(&e))?;
    write_stamp(file).map_err(|e| errno_of(&e))?;
    file.sync_all().map_err(|e| errno_of(&e))?;
    // Test-only fault injection point (see `POST_DATA_FAULT` below): fires,
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
// the fill's tail (`commit`): immediately before `state=hydrated` — the
// commit point — is written.
//
// moved that write to be the *last* fallible step, so that any
// earlier failure leaves the file demoted, punched and stamp-less rather
// than `hydrated` over zeros. The only host-reachable failure downstream of
// the data landing used to be `set_mtime`'s (a pre-epoch mtime), later
// correctly made non-fatal — which took away the one test able to
// reach this window without a VM. This hook restores it: a test installs a
// closure, `commit` calls it right before the commit write and, if it returns
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
mod tests;
