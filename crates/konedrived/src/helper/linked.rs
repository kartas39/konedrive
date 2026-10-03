//! What the daemon asks of the helper beyond the fills, as a trait: the account's link is one
//! implementation ([`Linked`]), a test's stand-in another.

use std::fs::File;
use std::os::fd::OwnedFd;

use async_trait::async_trait;
use konedrive_fs::handle::FileHandle;

use super::{Clearance, HelperError, HelperLink, LinkCell};

/// What a move out asks of the helper (`docs/design/writes.md` §8).
#[async_trait]
pub trait Helper: Send + Sync {
    /// `OpenByHandle` ([`HelperLink::open_by_handle`]).
    async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError>;
    async fn mark_file(&self, file: &File) -> Result<(), HelperError>;
    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError>;
    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError>;
    /// How a file that may carry an ignore mark is cleared before a fill that could fail and
    /// empty it (`source::hydrate_with`); `None` while there is no link.
    fn clearance(&self) -> Option<Clearance>;
}

/// The helper, over the account's link cell: `NotRunning` while there is no link.
pub struct Linked(pub LinkCell);

impl Linked {
    fn link(&self) -> Result<HelperLink, HelperError> {
        self.0.lock().unwrap().clone().ok_or(HelperError::NotRunning)
    }
}

#[async_trait]
impl Helper for Linked {
    async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        self.link()?.open_by_handle(dir, handle).await
    }

    async fn mark_file(&self, file: &File) -> Result<(), HelperError> {
        self.link()?.mark_file(file).await
    }

    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.link()?.mark_dir(dir).await
    }

    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.link()?.unmark_dir(dir).await
    }

    fn clearance(&self) -> Option<Clearance> {
        self.link().ok().map(Clearance::Link)
    }
}
