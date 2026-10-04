//! The local rule of every punch: no ignore mark of ours is left on a file that is emptied.

use std::fs::File;
use std::path::PathBuf;

use super::presence::{helper_presence, HelperPresence};
use super::{HelperError, HelperLink};

/// How a punch makes sure it leaves no ignore mark of ours on the file it
/// empties — **the local rule**, decided at the punch and nowhere
/// else.
///
/// An ignore mark on an emptied file lets every later open through to its
/// zeros, silently, for as long as the mark lives (it survives
/// modification). Whether one can be there used to be argued across the
/// whole system — "a folder without interception cannot carry a stale mark
/// that matters" — and that argument was falsified three times, each time by
/// a race nobody had seen (H132, the 's N2).
/// This does not argue at all. Right before a file is emptied:
///
/// - **a link to the helper exists** → the helper is asked to `ClearIgnore`,
///   and any reported failure stops the punch. The helper grants it on
///   ownership of the file alone, since removing a mark can only cost an
///   extra interception, never zeros;
/// - **no helper has its socket bound** → no fanotify group of ours exists,
///   so no mark of ours does ([`HelperPresence::Absent`]); the punch goes
///   ahead;
/// - **a helper is bound and this daemon has no link to it** → its group may
///   hold a mark nobody here can clear; nothing is emptied, and the caller
///   tries again once the link is up.
///
/// No new race can falsify it: it depends on nothing that happened before
/// the punch.
#[derive(Clone)]
pub enum Clearance {
    Link(HelperLink),
    /// No link: the helper's socket path, looked at when the punch is due.
    NoLink(PathBuf),
}

/// Why a [`Clearance`] did not clear the way for a punch.
#[derive(Debug, thiserror::Error)]
pub enum NotCleared {
    #[error("the helper did not clear the ignore mark: {0}")]
    Helper(HelperError),
    #[error(
        "a konedrive helper is running and this daemon is not connected to it yet, so the \
         file's ignore mark cannot be cleared"
    )]
    Unlinked,
    #[error("cannot tell whether a konedrive helper is running: {0}")]
    Unknown(String),
    /// A fill was given no [`Clearance`] at all for a file that is not
    /// `online-only` (`source::hydrate_with`): never an answer of
    /// [`Clearance::clear`].
    #[error("the file may carry an ignore mark, and there is no link to the konedrive helper to clear it with")]
    NoWay,
}

impl Clearance {
    /// Clears the way for emptying `file`, by the rule on [`Clearance`].
    /// `Ok` is the only answer after which a punch may follow.
    pub async fn clear(&self, file: &File) -> Result<(), NotCleared> {
        match self {
            Clearance::Link(link) => link.clear_ignore(file).await.map_err(NotCleared::Helper),
            Clearance::NoLink(socket) => {
                let socket = socket.clone();
                let presence = tokio::task::spawn_blocking(move || helper_presence(&socket))
                    .await
                    .unwrap_or_else(|e| HelperPresence::Unknown(e.to_string()));
                match presence {
                    HelperPresence::Absent => Ok(()),
                    HelperPresence::Present => Err(NotCleared::Unlinked),
                    HelperPresence::Unknown(why) => Err(NotCleared::Unknown(why)),
                }
            }
        }
    }
}
