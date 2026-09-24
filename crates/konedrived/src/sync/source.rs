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

use super::helper::{Clearance, HelperLink, NotCleared};
use crate::quickxor::QuickXor;

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
    pub quick_xor: Option<[u8; crate::quickxor::LEN]>,
}

#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Bytes of `item_id` starting at `from`, plus the item's current size and mtime.
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError>;
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
pub(super) fn refused_source_path(path: &Path, root: &Path) -> io::Result<Option<String>> {
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
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
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
            match fill_file(file, source, clearance.as_ref()).await {
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
    fill_file(File::from(fd), source, clearance).await
}

async fn fill_file(
    file: File,
    source: &dyn ContentSource,
    clearance: Option<&Clearance>,
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

    fill(&file, &item_id, original_size, source).await.map_err(|errno| {
        roll_back(&file, original_size, original.1);
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

async fn fill(file: &File, item_id: &str, original_size: u64, source: &dyn ContentSource) -> Result<(), i32> {
    let resume = usable_checkpoint(file, original_size);
    if resume.is_none() {
        drop_unusable_checkpoint(file)?;
    }
    let downloaded = download(file, item_id, original_size, source, resume, true).await?;
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
        let fetched = match source.fetch(item_id, written).await {
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
mod tests {
    use std::io::{Read, Seek};
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use konedrive_fs::placeholder::{
        read_ctag, read_progress, read_stamp, read_state, write_progress, Progress, State,
    };
    use konedrive_proto::ACCEPTED_DENY_ERRNOS;
    use tokio::io::ReadBuf;

    use crate::quickxor::QuickXor;

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
                version: None,
                stream,
            })
        }
    }

    /// Serves a real file but declares a pre-epoch mtime — one `futimens`
    /// cannot be given. Everything about the *content* is correct; only the
    /// timestamp is unrepresentable here.
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
    /// with nothing exercising it. `ENOENT`, the errno it
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

    /// I5, the positive half: a file completed across two
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

    /// I5, the negative half: the source answers the resume with
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

    /// I6: `written(0) >= size(0)` ends the fill loop on the
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

    /// An mtime the local filesystem cannot represent must not
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
    /// from birth and would refuse to dehydrate the file ever again,
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

    /// C1, pinned back onto the host suite. This is the test
    /// `an_mtime_the_filesystem_cannot_hold_still_hydrates_the_file` used to
    /// be, before correctly made `set_mtime`'s error non-fatal and
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

    /// A roll-back never punches a file that is no longer in the
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

        roll_back(&file, 4096, None);

        let mut content = Vec::new();
        std::fs::File::open(dir.path().join("file.bin")).unwrap().read_to_end(&mut content).unwrap();
        assert!(content == vec![6u8; 4096], "the file's content must be left alone");
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated), "and so must its state");
    }

    /// A byte stream that fails with a connection error at `fail_at` (an
    /// offset into `data`), the way a dropped HTTP connection does. It hands
    /// out at most 16 KiB per read, as a socket does, so that a fill's
    /// checkpoints land where they would in a real download rather than
    /// being skipped over by one read the size of the whole buffer.
    struct Breaking {
        data: Vec<u8>,
        at: usize,
        fail_at: Option<usize>,
    }

    impl AsyncRead for Breaking {
        fn poll_read(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            if self.fail_at == Some(self.at) {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection reset by peer")));
            }
            let limit = self.fail_at.unwrap_or(self.data.len()).min(self.data.len());
            let end = limit.min(self.at + buf.remaining()).min(self.at + 16 * 1024);
            let chunk = self.data[self.at..end].to_vec();
            buf.put_slice(&chunk);
            self.at = end;
            Poll::Ready(Ok(()))
        }
    }

    /// Serves a file from memory as a Graph source would — a cTag and a
    /// quickXorHash with every answer — with the faults a download has to
    /// survive. `second` is served from fetch number `switch_at` on (a new
    /// version uploaded mid-download); `breaks` maps a fetch number to the
    /// absolute offset its stream breaks at; `wrong_hash` makes every answer
    /// carry a hash nothing matches, and `no_hash` makes every answer carry
    /// none at all. Records where each fetch started.
    struct Scripted {
        first: (String, Vec<u8>),
        second: Option<(String, Vec<u8>)>,
        switch_at: usize,
        breaks: std::collections::HashMap<usize, u64>,
        wrong_hash: bool,
        no_hash: bool,
        froms: Mutex<Vec<u64>>,
        /// The file being filled, when a test wants to know what checkpoint
        /// it carried at the moment each fetch was made.
        watching: Option<std::fs::File>,
        progress_seen: Mutex<Vec<Option<Progress>>>,
    }

    impl Scripted {
        fn new(ctag: &str, data: Vec<u8>) -> Self {
            Self { first: (ctag.into(), data), second: None, switch_at: usize::MAX, breaks: Default::default(), wrong_hash: false, no_hash: false, froms: Mutex::new(Vec::new()), watching: None, progress_seen: Mutex::new(Vec::new()) }
        }

        fn froms(&self) -> Vec<u64> {
            self.froms.lock().unwrap().clone()
        }

        fn progress_seen(&self) -> Vec<Option<Progress>> {
            self.progress_seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ContentSource for Scripted {
        async fn fetch(&self, _item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            let n = {
                let mut froms = self.froms.lock().unwrap();
                froms.push(from);
                froms.len() - 1
            };
            if let Some(watched) = &self.watching {
                self.progress_seen.lock().unwrap().push(read_progress(watched).unwrap());
            }
            let (ctag, data) = match &self.second {
                Some(second) if n >= self.switch_at => second,
                _ => &self.first,
            };
            let hash = if self.wrong_hash {
                [0x55u8; 20]
            } else {
                let mut h = QuickXor::new();
                h.update(data);
                h.finish()
            };
            let start = (from as usize).min(data.len());
            let fail_at = self.breaks.get(&n).map(|&at| (at as usize).saturating_sub(start));
            Ok(Fetched {
                served_from: from,
                size: data.len() as u64,
                mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
                version: Some(Version { ctag: ctag.clone(), quick_xor: (!self.no_hash).then_some(hash) }),
                stream: Box::new(Breaking { data: data[start..].to_vec(), at: 0, fail_at }),
            })
        }
    }

    fn content(size: usize, seed: u8) -> Vec<u8> {
        (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn read_back(file: &std::fs::File) -> Vec<u8> {
        use std::os::unix::fs::FileExt;
        let mut out = vec![0u8; file.metadata().unwrap().len() as usize];
        file.read_exact_at(&mut out, 0).unwrap();
        out
    }

    #[tokio::test]
    async fn a_verified_fill_records_its_ctag_and_leaves_no_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(300_000, 1);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let source = Scripted::new("c1", data.clone());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(read_back(&file), data);
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
        assert_eq!(read_progress(&file).unwrap(), None);
    }

    #[tokio::test]
    async fn content_that_does_not_match_its_hash_is_fetched_once_more_then_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(300_000, 2);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let source = Scripted { wrong_hash: true, ..Scripted::new("c1", data) };
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
        assert_eq!(source.froms(), vec![0, 0], "one more try from the start, and no third");
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert!(read_stamp(&file).unwrap().is_none());
        assert_eq!(read_progress(&file).unwrap(), None);
    }

    /// The same, with checkpoints small enough that the second download
    /// makes some: content that has failed its hash twice is no prefix to
    /// continue from, so the refusal drops the checkpoint and the roll-back
    /// punches everything — the next open downloads afresh instead of from a
    /// prefix already known to belong to content that does not verify.
    #[tokio::test]
    async fn a_second_mismatch_drops_the_checkpoint_it_made() {
        set_checkpoint_every(64 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 14);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let source = Scripted { wrong_hash: true, ..Scripted::new("c1", data) };
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
        clear_checkpoint_every();

        assert_eq!(source.froms(), vec![0, 0]);
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_progress(&file).unwrap(), None, "no checkpoint survives a second mismatch");
        assert!(read_back(&file).iter().all(|b| *b == 0), "and none of the content that failed it");
    }

    /// case: with no quickXorHash nothing can check a resumed
    /// prefix, so a checkpoint is not continued from — here one whose first
    /// 8 KiB were lost is downloaded again from the start, not committed as
    /// correct with 8 KiB of zeros in it.
    #[tokio::test]
    async fn a_checkpoint_is_not_trusted_without_a_hash() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 15);
        let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
        file.write_all_at(&[0u8; 8192], 0).unwrap();
        let source = Scripted { no_hash: true, ..Scripted::new("c1", data.clone()) };
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![192 * 1024, 0], "the checkpoint is dropped, not continued");
        assert_eq!(read_back(&file), data);
        assert_eq!(read_progress(&file).unwrap(), None);
    }

    /// And a download without a hash makes no checkpoint to begin with: when
    /// it gives up, nothing of it is kept, and everything is punched as in
    /// part 1.
    #[tokio::test]
    async fn a_download_without_a_hash_writes_no_checkpoint() {
        set_checkpoint_every(64 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 16);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let mut source = Scripted { no_hash: true, ..Scripted::new("c1", data) };
        source.watching = Some(file.try_clone().unwrap());
        for n in 0..3 {
            source.breaks.insert(n, 200 * 1024);
        }
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
        clear_checkpoint_every();

        assert_eq!(source.froms(), vec![0, 200 * 1024, 200 * 1024]);
        assert_eq!(source.progress_seen(), vec![None, None, None], "no checkpoint while it downloaded");
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_progress(&file).unwrap(), None);
        assert!(read_back(&file).iter().all(|b| *b == 0), "everything that arrived is punched");
    }

    #[tokio::test]
    async fn a_dropped_connection_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(300_000, 3);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let mut source = Scripted::new("c1", data.clone());
        source.breaks.insert(0, 100_000);
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![0, 100_000]);
        assert_eq!(read_back(&file), data);
    }

    #[tokio::test]
    async fn a_file_changed_in_the_cloud_mid_download_starts_over_with_the_new_version() {
        let dir = tempfile::tempdir().unwrap();
        let old = content(300_000, 4);
        let new = content(250_000, 5);
        let file = placeholder(dir.path(), "I", old.len() as u64);
        let mut source = Scripted::new("c1", old);
        source.second = Some(("c2".into(), new.clone()));
        source.switch_at = 1;
        source.breaks.insert(0, 100_000);
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![0, 100_000, 0]);
        assert_eq!(read_back(&file), new);
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    }

    /// The cTag is the only thing that notices a new version when OneDrive
    /// gives no quickXorHash: the old version's first bytes and
    /// the new one's tail would otherwise be committed as one file.
    #[tokio::test]
    async fn a_file_changed_mid_download_starts_over_even_without_a_hash() {
        let dir = tempfile::tempdir().unwrap();
        let old = content(300_000, 12);
        let new = content(250_000, 13);
        let file = placeholder(dir.path(), "I", old.len() as u64);
        let mut source = Scripted { no_hash: true, ..Scripted::new("c1", old) };
        source.second = Some(("c2".into(), new.clone()));
        source.switch_at = 1;
        source.breaks.insert(0, 100_000);
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![0, 100_000, 0]);
        assert_eq!(read_back(&file), new, "no byte of the old version may survive into the new one");
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    }

    /// A download that gives up keeps what it made durable.
    #[tokio::test]
    async fn a_download_that_gives_up_keeps_its_checkpoint() {
        set_checkpoint_every(64 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 6);
        let file = placeholder(dir.path(), "I", data.len() as u64);
        let mut source = Scripted::new("c1", data.clone());
        for n in 0..3 {
            source.breaks.insert(n, 200 * 1024);
        }
        assert_eq!(hydrate_file(&file, &source).await, libc::EIO);
        clear_checkpoint_every();

        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert!(read_stamp(&file).unwrap().is_none());
        assert_eq!(read_progress(&file).unwrap(), Some(Progress { ctag: "c1".into(), bytes: 192 * 1024 }));
        let back = read_back(&file);
        assert_eq!(&back[..192 * 1024], &data[..192 * 1024], "the checkpointed prefix is kept");
        assert!(back[192 * 1024..].iter().all(|b| *b == 0), "everything past it is punched");
        assert!(file.metadata().unwrap().blocks() * 512 < 400 * 1024, "the tail's blocks are freed");
    }

    #[tokio::test]
    async fn a_checkpoint_is_resumed_and_the_whole_file_verified() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 7);
        let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
        let source = Scripted::new("c1", data.clone());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![192 * 1024], "it continued from the checkpoint");
        assert_eq!(read_back(&file), data);
        assert_eq!(read_progress(&file).unwrap(), None);
    }

    /// The resume is safe because the hash covers what lay on disk too.
    #[tokio::test]
    async fn a_damaged_checkpoint_is_caught_by_the_hash_and_downloaded_again() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 8);
        let file = checkpointed(dir.path(), &data, "c1", 192 * 1024);
        file.write_all_at(&[data[10] ^ 0xff], 10).unwrap();
        let source = Scripted::new("c1", data.clone());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![192 * 1024, 0]);
        assert_eq!(read_back(&file), data);
    }

    #[tokio::test]
    async fn a_checkpoint_for_another_version_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(1 << 20, 9);
        let file = checkpointed(dir.path(), &data, "c0", 192 * 1024);
        let source = Scripted::new("c1", data.clone());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![192 * 1024, 0]);
        assert_eq!(read_back(&file), data);
    }

    #[tokio::test]
    async fn a_checkpoint_past_the_end_of_the_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(100_000, 10);
        let file = checkpointed(dir.path(), &data, "c1", 100_000);
        write_progress(&file, &Progress { ctag: "c1".into(), bytes: 200_000 }).unwrap();
        let mut source = Scripted::new("c1", data.clone());
        source.watching = Some(file.try_clone().unwrap());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![0]);
        // Gone before the first byte is asked for — not merely by the commit
        // at the end: new bytes must not go under a count that recovery
        // could adopt after a crash.
        assert_eq!(source.progress_seen(), vec![None], "the unusable checkpoint was still there");
        assert_eq!(read_progress(&file).unwrap(), None);
    }

    #[tokio::test]
    async fn a_checkpoint_at_the_very_end_needs_no_more_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let data = content(100_000, 11);
        let file = checkpointed(dir.path(), &data, "c1", 100_000);
        let source = Scripted::new("c1", data.clone());
        assert_eq!(hydrate_file(&file, &source).await, 0);
        assert_eq!(source.froms(), vec![100_000]);
        assert_eq!(read_back(&file), data);
    }

    /// A placeholder as a download that gave up at `bytes` leaves it.
    fn checkpointed(dir: &std::path::Path, data: &[u8], ctag: &str, bytes: u64) -> std::fs::File {
        use std::os::unix::fs::FileExt;
        let file = placeholder(dir, "I", data.len() as u64);
        file.write_all_at(&data[..bytes as usize], 0).unwrap();
        write_progress(&file, &Progress { ctag: ctag.into(), bytes }).unwrap();
        file
    }
}
