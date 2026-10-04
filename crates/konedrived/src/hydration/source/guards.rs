//! What every download checks, whether it runs as one stream (`download`) or in parts
//! (`parts`): the answer of the source before a byte of it is written, the breaks that are
//! tried again, the checkpoint a download may continue from, the hash at the end, and the
//! one time a download starts over.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::time::Duration;

use konedrive_fs::placeholder::{remove_progress, Progress};
use konedrive_graph::quickxor::QuickXor;
use konedrive_proto::clamp_deny_errno;

use super::target::Target;
use super::{ContentSource, Fetched, SourceError, Version};

/// How often a fill makes its progress durable.
const CHECKPOINT_EVERY: u64 = 16 * 1024 * 1024;

/// The pause after a download's first break; the second is twice as long.
const BACK_OFF: Duration = Duration::from_millis(200);

/// How many breaks end a download.
const BREAKS: u32 = 3;

/// The bytes a stream reads at once, and a checkpoint's prefix is read back in.
pub(super) const BUFFER: usize = 256 * 1024;

/// The numbers of a download that a test sets smaller.
#[derive(Debug, Clone, Copy)]
pub(super) struct Tuning {
    /// The bytes between two checkpoints; `None` for a download that makes none
    /// (`download_into`: a file nobody else can see does not survive a crash).
    pub checkpoint_every: Option<u64>,
    /// The pause after the first break.
    pub back_off: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { checkpoint_every: Some(CHECKPOINT_EVERY), back_off: BACK_OFF }
    }
}

/// Maps a local filesystem failure onto the errno the suspended `open()` is answered with.
///
/// A **clamp**, not a flattening: `ENOSPC` and `EDQUOT` are in the kernel's accepted
/// `FAN_DENY` set and are exactly what a `pwrite` or an `fsync` produces on a full disk or
/// an exhausted quota. Everything else the local filesystem can report (`EROFS`, `EBADF`,
/// `EFBIG`, ...) is outside the set and would make the helper's response `write()` fail
/// with `EINVAL`, leaving the opener suspended forever, so it becomes `EIO`.
pub(super) fn errno_of(e: &io::Error) -> i32 {
    clamp_deny_errno(e.raw_os_error().unwrap_or(libc::EIO))
}

/// Why an attempt at the whole file ended without it.
pub(super) enum End {
    /// Answered with this errno; nothing more is tried. Every value is in
    /// `konedrive_proto::ACCEPTED_DENY_ERRNOS`: a missing remote item and a failure that
    /// did not pass are `EIO`, never the errno their cause might suggest (`ENOENT`,
    /// `ECONNRESET`), which the kernel would not deliver.
    Fail(i32),
    /// The file changed in the cloud mid-download: start over, once.
    Changed,
    /// The content does not match the file's quickXorHash: start over, once.
    Mismatch,
    /// The checkpoint cannot be continued from: download from the start (not a start over).
    DropCheckpoint,
}

impl From<io::Error> for End {
    fn from(e: io::Error) -> Self {
        End::Fail(errno_of(&e))
    }
}

/// The breaks of one run of answers: a fetch the source says failed in passing, a stream
/// that ended early or with an error. Each is tried again after a pause; the third ends the
/// download.
#[derive(Debug, Clone, Copy)]
pub(super) struct Breaks {
    count: u32,
    back_off: Duration,
}

impl Breaks {
    pub(super) fn new(tuning: &Tuning) -> Self {
        Self { count: 0, back_off: tuning.back_off }
    }

    /// One more break, `why`: waits before the next try, or gives up.
    pub(super) async fn again(&mut self, item_id: &str, why: &str) -> Result<(), End> {
        self.count += 1;
        if self.count >= BREAKS {
            tracing::warn!("{item_id}: giving up after {BREAKS} breaks: {why}");
            return Err(End::Fail(libc::EIO));
        }
        tokio::time::sleep(self.back_off * self.count).await;
        Ok(())
    }
}

/// A fetch of `[from, end)`, tried again while the source says the failure is passing.
pub(super) async fn open(
    source: &dyn ContentSource,
    item_id: &str,
    from: u64,
    end: Option<u64>,
    breaks: &mut Breaks,
) -> Result<Fetched, End> {
    loop {
        match source.fetch(item_id, from, end).await {
            Ok(fetched) => return Ok(fetched),
            Err(SourceError::NotFound(_)) => return Err(End::Fail(libc::EIO)),
            Err(SourceError::Transient(why)) => breaks.again(item_id, &why).await?,
        }
    }
}

/// Refuses an answer no byte of which may be written.
///
/// - Bytes are written where the source says they start, or not at all. A source that
///   answers a resume by restarting the body at 0 (an HTTP `200` to a `Range` request)
///   would otherwise have the file's beginning written over its middle, and the result
///   reported as a success.
/// - A declared size of 0 against a placeholder that carries a real size (`original_size`)
///   is refused, not obeyed: obeying it truncates live content on the strength of one
///   unconfirmed answer. A file emptied in the cloud is a change of metadata, and belongs
///   to the sync, not to a fill started by an open. Every other resize, in either
///   direction, goes through.
pub(super) fn check_answer(item_id: &str, fetched: &Fetched, asked_from: u64, original_size: u64) -> Result<(), End> {
    if fetched.served_from != asked_from {
        tracing::error!(
            "{item_id}: asked for byte {asked_from} and got a stream starting at {}; refusing rather \
             than write it at the wrong offset",
            fetched.served_from
        );
        return Err(End::Fail(libc::EIO));
    }
    if fetched.size == 0 && original_size != 0 {
        tracing::error!(
            "{item_id}: the source declares size 0 for a placeholder of {original_size} bytes; \
             refusing to truncate it here"
        );
        return Err(End::Fail(libc::EIO));
    }
    Ok(())
}

pub(super) fn same_version(a: &Option<Version>, b: &Option<Version>) -> bool {
    a.as_ref().map(|v| &v.ctag) == b.as_ref().map(|v| &v.ctag)
}

/// The hash a download continues with from the checkpoint `progress`, given the version
/// and the size the first answer declares: the first `progress.bytes` of the file, read back from disk.
///
/// A checkpoint is continued only for the same version, only when that version has a
/// quickXorHash — nothing else vouches for bytes that lay on disk across a failure or a
/// crash — and only when the file is still as long as the bytes it counts.
/// [`End::DropCheckpoint`] for any other, and for a prefix that cannot be read back.
pub(super) async fn resume(
    target: &Target,
    item_id: &str,
    progress: &Progress,
    version: &Option<Version>,
    size: u64,
) -> Result<QuickXor, End> {
    let continues =
        version.as_ref().is_some_and(|v| v.ctag == progress.ctag && v.quick_xor.is_some()) && progress.bytes <= size;
    let rebuilt = if continues {
        let bytes = progress.bytes;
        target.alone(move |file| rehash(file, bytes)).await
    } else {
        None
    };
    rebuilt.ok_or_else(|| {
        tracing::info!(
            "{item_id}: the checkpoint at byte {} is for another version, has no hash to be checked \
             against, or cannot be read back; downloading from the start",
            progress.bytes
        );
        End::DropCheckpoint
    })
}

/// The hash of the first `bytes` of `file`, read back from disk. `None` if they cannot all
/// be read.
fn rehash(file: &File, bytes: u64) -> Option<QuickXor> {
    let mut buffer = vec![0u8; BUFFER];
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

/// The whole file is down: its hash against the version's. [`End::Mismatch`] when they
/// differ; a version without a hash cannot be checked, and passes with a warning.
pub(super) fn verify(item_id: &str, version: &Option<Version>, hash: &QuickXor) -> Result<(), End> {
    match version.as_ref().map(|v| v.quick_xor) {
        Some(Some(want)) if hash.finish() != want => Err(End::Mismatch),
        Some(None) => {
            tracing::warn!("{item_id}: OneDrive gave no quickXorHash; the content could not be verified");
            Ok(())
        }
        _ => Ok(()),
    }
}

/// What follows an attempt that ended without the file: the errno the download ends with,
/// or `Ok` to make another attempt from the start.
///
/// - [`End::Changed`] and [`End::Mismatch`] start over once between them; the second is
///   `EIO`.
/// - A checkpoint goes whenever its bytes are no prefix of what the next attempt downloads
///   — and after a mismatch even when no attempt follows: it counts bytes of content that
///   failed the hash, and the roll-back would keep them for the next fill.
#[derive(Default)]
pub(super) struct StartOver {
    used: bool,
}

impl StartOver {
    pub(super) async fn after(&mut self, end: End, target: &Target, item_id: &str, tuning: &Tuning) -> Result<(), i32> {
        let checkpoints = tuning.checkpoint_every.is_some();
        let drop_checkpoint = || async {
            if checkpoints {
                target.alone(remove_progress).await.map_err(|e| errno_of(&e))?;
            }
            Ok::<_, i32>(())
        };
        match end {
            End::Fail(errno) => Err(errno),
            End::DropCheckpoint => drop_checkpoint().await,
            End::Changed => {
                if self.used {
                    tracing::error!("{item_id}: the file keeps changing in the cloud while it downloads");
                    return Err(libc::EIO);
                }
                tracing::info!("{item_id}: the file changed in the cloud mid-download; starting over");
                self.used = true;
                drop_checkpoint().await
            }
            End::Mismatch => {
                drop_checkpoint().await?;
                if self.used {
                    tracing::error!("{item_id}: the content does not match its quickXorHash, twice");
                    return Err(libc::EIO);
                }
                tracing::warn!("{item_id}: the content does not match its quickXorHash; downloading it once more");
                self.used = true;
                Ok(())
            }
        }
    }
}
