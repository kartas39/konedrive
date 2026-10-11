//! A stale mount: the mount a daemon that died left on a folder. Every path in the folder
//! then fails with "not connected", and nothing can be mounted over it.
//!
//! **How one is told.** Not by the folder's attributes: the kernel remembers those for
//! the time the daemon said (`ATTRIBUTE_LIFETIME`, a second), and answers them from
//! memory after the daemon's death — a tidy step run at once, or a daemon started at
//! once, saw a folder that looked served. Two things are asked instead: the kernel's
//! table of mounts, whether a FUSE mount lies over the folder at all; and of such a mount
//! its filesystem statistics (`statfs`), which the kernel remembers for nobody and hands
//! to the mount's daemon each time. A mount whose daemon is gone refuses it at once
//! ("not connected"); one whose daemon runs answers it.
//!
//! **It never waits on a mount.** The question is asked on a thread of its own and
//! waited for [`STATISTICS_WAIT`]: a daemon that runs and does not answer in that time is
//! not dead, and its mount is left alone — the thread stays until the answer comes.
//!
//! **Tested without a mount:** [`decide_is_mount_stale`], a case each.
// test: a_mount_is_stale_when_its_statistics_are_refused_as_not_connected

use std::path::Path;
use std::time::Duration;

use super::tidy_step::is_mounted;

/// How long the statistics of a mount are waited for.
const STATISTICS_WAIT: Duration = Duration::from_secs(2);

/// What came of asking a mount for its filesystem statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatisticsAnswer {
    /// The mount's daemon answered.
    Answered,
    /// The request was refused, with this errno.
    Refused { errno: i32 },
    /// No answer came within [`STATISTICS_WAIT`]: a daemon runs, and is slow.
    NoneInTime,
}

/// Whether a FUSE mount whose statistics were answered so is stale: its connection to a
/// daemon is gone (`ENOTCONN`, or `ECONNABORTED` for the request that was under way when
/// it went). Any other refusal, and no answer in time, say nothing of a dead daemon: the
/// mount is left. Pure.
pub(super) fn decide_is_mount_stale(answer: StatisticsAnswer) -> bool {
    match answer {
        StatisticsAnswer::Refused { errno } => errno == libc::ENOTCONN || errno == libc::ECONNABORTED,
        StatisticsAnswer::Answered | StatisticsAnswer::NoneInTime => false,
    }
}

/// Asks the filesystem at `folder` for its statistics, on a thread of its own, and waits
/// for the answer no longer than [`STATISTICS_WAIT`].
fn ask_statistics(folder: &Path) -> StatisticsAnswer {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(folder.as_os_str().as_bytes()) else { return StatisticsAnswer::Refused { errno: libc::EINVAL } };
    let (answered, answer) = std::sync::mpsc::channel();
    let asked = std::thread::Builder::new().name("konedrive-mount-statistics".into()).spawn(move || {
        let mut statistics = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `path` is a C string that lives through the call, and `statistics` is room for one `statfs`.
        let refused = unsafe { libc::statfs(path.as_ptr(), statistics.as_mut_ptr()) } != 0;
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let _ = answered.send(if refused { StatisticsAnswer::Refused { errno } } else { StatisticsAnswer::Answered });
    });
    if asked.is_err() {
        return StatisticsAnswer::NoneInTime;
    }
    answer.recv_timeout(STATISTICS_WAIT).unwrap_or(StatisticsAnswer::NoneInTime)
}

/// Whether `folder` carries the mount of a daemon that is gone: a FUSE mount lies over it
/// and refuses its statistics as not connected. For a folder the table of mounts does not
/// name (the table could not be read), what the folder's own attributes say is left to go
/// by.
fn is_stale_mount_at(folder: &Path) -> bool {
    if is_mounted(folder) {
        return decide_is_mount_stale(ask_statistics(folder));
    }
    std::fs::metadata(folder).is_err_and(|error| error.raw_os_error() == Some(libc::ENOTCONN))
}

/// Takes off the stale mount on `folder`, when there is one.
pub fn unmount_stale_mount(folder: &Path) {
    if is_stale_mount_at(folder) {
        tracing::warn!("{} carries the mount of a daemon that is gone; taking it off", folder.display());
        let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(folder).status();
    }
}

#[cfg(test)]
mod tests;
