//! What a download whose content was fetched whole reports, for how its landing went, and
//! whether the store still has the file at the version that was downloaded: decisions, with
//! no disk in them.

use konedrive_graph::quickxor::decode_base64;
use konedrive_tree::Row;

use crate::fuse::backing_directory::LandingOutcome;
use crate::hydration::source::Version;

/// How the landing of a download went, once its content was fetched whole and checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LandingAttempt {
    /// The file was moved or removed here while it was downloaded: the download is of no
    /// path now, and nothing was landed.
    FileMovedMeanwhile,
    /// The store's row for the file has another version than the one downloaded: a cycle
    /// brought another content of the file while it was downloaded, or OneDrive has one the
    /// store does not have yet. Nothing was landed.
    OneDriveVersionChanged,
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
/// - The store has another version of the file than the one downloaded: `ESTALE` too — what
///   was asked for is no longer what the folder shows, and the next read starts a download
///   of the version the store has then.
pub(super) fn to_download_result(attempt: LandingAttempt) -> Result<(), i32> {
    match attempt {
        LandingAttempt::Attempted(LandingOutcome::Placed | LandingOutcome::Kept) => Ok(()),
        LandingAttempt::Attempted(LandingOutcome::Refused) => Err(libc::EEXIST),
        LandingAttempt::FileMovedMeanwhile | LandingAttempt::OneDriveVersionChanged => Err(libc::ESTALE),
    }
}

/// Whether `stored`, the store's row of a OneDrive file now, is of the version that was
/// downloaded: `asked` is the row the download was asked with, and `downloaded` the version
/// the source gave the content as and the download checked it against, `None` when the
/// source said none.
///
/// - The store's row has another content than the row that was asked with — another ctag,
///   hash or size, as a cycle tells new content: no.
/// - The source gave a version: it is the store's when both have a quickXorHash and the
///   hashes are equal; when either has none, when the store's row has no ctag or the ctags
///   are equal. Any other: no.
/// - The source gave none: the rows alone say it.
pub(super) fn has_downloaded_version(stored: &Row, asked: &Row, downloaded: Option<&Version>) -> bool {
    let has_content_asked_for = stored.ctag == asked.ctag && stored.quickxor == asked.quickxor && stored.size == asked.size;
    has_content_asked_for
        && downloaded.is_none_or(|downloaded| match (stored.quickxor.as_deref().and_then(decode_base64), downloaded.quick_xor) {
            (Some(stored_hash), Some(downloaded_hash)) => stored_hash == downloaded_hash,
            (None, _) | (_, None) => stored.ctag.as_deref().is_none_or(|ctag| ctag == downloaded.ctag),
        })
}

#[cfg(test)]
mod tests;
