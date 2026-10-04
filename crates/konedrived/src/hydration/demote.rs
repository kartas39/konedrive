//! Turning a file back into a placeholder: the one place in the daemon that empties a file
//! of ours and calls it `online-only`.
//!
//! Four things end here: a fill that failed (`source::fill`), a download stopped because
//! its item was removed ([`back_to_placeholder`]), a free-up (`dehydrate`) and startup
//! recovery (`recovery`). They differ in what they keep and in what holds the file still,
//! and in nothing else.
//!
//! # The order, and why it is safe
//!
//! Only a file that reads `hydrating` or `dehydrating` is emptied. Both say "the content is
//! not to be trusted": the helper fills such a file on its next open instead of letting the
//! opener through, never places an ignore mark on it, and startup recovery resets it. So:
//!
//! 1. the state is read, and a file in any other state is left exactly as it is;
//! 2. the checkpoint is dropped, unless its prefix is kept — the attribute before the punch:
//!    a count of bytes never outlives the bytes it counts;
//! 3. the punch, the size, the times;
//! 4. `fsync`;
//! 5. `online-only`, and the stamp off.
//!
//! A failure or a crash before step 5 leaves the file in the state it was found in, which
//! the next open or the next start takes from there. After step 5 it is a placeholder. At
//! no point does a file read `hydrated` or `online-only` while it holds something else than
//! that state says — except a kept prefix, which is counted by its checkpoint and checked
//! against the file's hash by the fill that continues from it.
//!
//! What has to be true *before* the call is the caller's: the file's ignore mark was
//! cleared, or none can be on it (`helper::Clearance`), and nothing else writes the file
//! ([`Held`]).

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{
    punch_all, punch_from, read_progress, read_state, remove_progress, remove_stamp, reopen_writable, write_state,
    Progress, State, XATTR_PROGRESS,
};

/// What stays of the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    Nothing,
    /// The prefix a download's checkpoint counts, with the checkpoint, when the file has a
    /// usable one ([`usable_checkpoint`]) and was found `hydrating`; nothing otherwise.
    Checkpoint,
}

/// What holds the file still while it is emptied, which decides the states it may be
/// found in.
pub(crate) enum Held<'a> {
    /// A write lease: nobody else has the file open. A file found `hydrating` or
    /// `dehydrating` is emptied. The lease is asked for as the proof, and not used.
    Lease(#[allow(dead_code)] &'a WriteLease<'a>),
    /// The fill that wrote `hydrating`, under the per-inode lock, with its opener
    /// suspended: no lease can be had (the opener's descriptor refuses it) and none is
    /// needed. Only a file still `hydrating` is emptied.
    Fill,
}

/// What the file is to look like afterwards.
pub(crate) struct Shape {
    /// The size to give it; `None` keeps the one it has.
    pub size: Option<u64>,
    /// The times to give it: a punch moves the mtime to now, and a placeholder whose time
    /// is not the cloud's has its thumbnail refused by KIO, which then opens the file to
    /// make one of its own — a download.
    pub times: FileTimes,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Demoted {
    /// A placeholder again, holding the first `kept` bytes of its content.
    Done { kept: u64 },
    /// Not in a state this call may empty: left exactly as it is.
    Left(Option<State>),
}

/// The times of a file, kept so they can be put back after a punch.
#[derive(Clone, Copy)]
pub(crate) struct FileTimes {
    atime: libc::timespec,
    mtime: libc::timespec,
}

impl FileTimes {
    pub(crate) fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            atime: libc::timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
            mtime: libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
        })
    }

    /// Puts both back on the same descriptor: the two raw `timespec`s the file carried, so
    /// a time before 1970 or one with nanoseconds comes back exactly.
    pub(crate) fn restore(self, file: &File) -> io::Result<()> {
        let times = [self.atime, self.mtime];
        // SAFETY: `file` is an open descriptor and `times` is a live array of exactly the
        // two `timespec`s `futimens` reads.
        let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// The checkpoint a download left on `file`, if a download can continue from it: well
/// formed, counting at least one byte, and not past `size` — a file that short cannot hold
/// the bytes it counts.
pub(crate) fn usable_checkpoint(file: &File, size: u64) -> Option<Progress> {
    match read_progress(file) {
        Ok(Some(progress)) if progress.bytes > 0 && progress.bytes <= size => Some(progress),
        _ => None,
    }
}

/// Takes the checkpoint off `file`, whether it reads as one or not. Only when the
/// attribute is there: a removal lifts a locked file's write bit for a moment
/// (`with_owner_write`), which a file without one gives no reason for. Whether there was
/// one.
pub(crate) fn drop_checkpoint(file: &File) -> io::Result<bool> {
    if let Ok(None) = xattr::FileExt::get_xattr(file, XATTR_PROGRESS) {
        return Ok(false);
    }
    remove_progress(file)?;
    Ok(true)
}

/// Turns `file` back into a placeholder, in the order the module doc gives. `file` must be
/// open for writing.
///
/// An error leaves the file in the state it was found in — `hydrating` or `dehydrating`,
/// with whatever the steps before the failure did to its content — and never `online-only`:
/// the next open fills it, or the next start resets it.
pub(crate) fn demote(file: &File, keep: Keep, shape: Shape, held: Held<'_>) -> io::Result<Demoted> {
    let found = read_state(file).map_err(|e| io::Error::other(e.to_string()))?;
    let may = matches!((&held, found), (_, Some(State::Hydrating)) | (Held::Lease(_), Some(State::Dehydrating)));
    if !may {
        return Ok(Demoted::Left(found));
    }
    let size = match shape.size {
        Some(size) => size,
        None => file.metadata()?.len(),
    };
    // A file found `dehydrating` was whole: a checkpoint on it counts nothing.
    let kept = match (keep, found) {
        (Keep::Checkpoint, Some(State::Hydrating)) => usable_checkpoint(file, size).map_or(0, |p| p.bytes),
        _ => 0,
    };
    if kept == 0 {
        drop_checkpoint(file)?;
        punch_all(file)?;
    } else {
        punch_from(file, kept)?;
    }
    if shape.size.is_some() {
        file.set_len(size)?;
    }
    // Before the fsync, so that it covers the times too.
    shape.times.restore(file)?;
    file.sync_all()?;

    write_state(file, State::OnlineOnly)?;
    // The stamp described a downloaded file. By now the file is a placeholder whatever
    // becomes of the stamp, so its removal is reported and nothing is undone.
    remove_stamp(file).map_err(|e| {
        io::Error::new(e.kind(), format!("the file is now online-only, but its stamp could not be removed: {e}"))
    })?;
    Ok(Demoted::Done { kept })
}

/// A file whose download was stopped part-way because its item was removed (issue #104),
/// and which survives — set aside for another account, or left by a removal that failed: a
/// placeholder again, with no content and no checkpoint, never a partly filled file. Only a
/// file still `hydrating` is touched; the caller holds its inode lock, and the fill that
/// wrote `hydrating` had its way cleared.
///
/// It keeps the time it has: that of the download's last write, as after startup recovery.
pub(crate) fn back_to_placeholder(file: &File) {
    if !matches!(read_state(file), Ok(Some(State::Hydrating))) {
        return;
    }
    let demoted = reopen_writable(file).and_then(|writable| {
        let shape = Shape { size: None, times: FileTimes::of(&writable)? };
        demote(&writable, Keep::Nothing, shape, Held::Fill)
    });
    match demoted {
        Ok(Demoted::Done { .. }) => {}
        Ok(Demoted::Left(now)) => tracing::info!("a stopped download reads {now:?} by now; left as it is"),
        Err(e) => tracing::error!(
            "cannot turn a stopped download back into a placeholder ({e}); it is left `hydrating`, \
             which the next open or the next start takes from there"
        ),
    }
}

#[cfg(test)]
mod tests;
