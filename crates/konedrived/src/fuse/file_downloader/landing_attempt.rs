//! What a download whose content was fetched whole reports, for how its landing went: a
//! decision, with no disk in it.

use crate::fuse::backing_directory::LandingOutcome;

/// How the landing of a download went, once its content was fetched whole and checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LandingAttempt {
    /// The file was moved or removed here while it was downloaded: the download is of no
    /// path now, and nothing was landed.
    FileMovedMeanwhile,
    /// The file was still where the download was asked for, and the backing directory was
    /// given the content to land there.
    Attempted(LandingOutcome),
}

/// What the download reports to whoever waits for it: that the file is downloaded, or the
/// errno it failed with.
///
/// - Placed at the path, or kept off it because the path holds the file's downloaded copy
///   already: downloaded.
/// - Refused, because the path holds something that is not the download's to replace:
///   `EEXIST`.
/// - The file moved meanwhile: `ESTALE`, and the next read starts a download where the file
///   is.
pub(super) fn to_download_result(attempt: LandingAttempt) -> Result<(), i32> {
    match attempt {
        LandingAttempt::Attempted(LandingOutcome::Placed | LandingOutcome::Kept) => Ok(()),
        LandingAttempt::Attempted(LandingOutcome::Refused) => Err(libc::EEXIST),
        LandingAttempt::FileMovedMeanwhile => Err(libc::ESTALE),
    }
}

#[cfg(test)]
mod tests;
