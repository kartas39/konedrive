//! "Always keep on this device": pins put on and taken off. A pin is a row of the folder's
//! tree store, kept by its mount, which downloads what a pin keeps.

use std::path::{Path, PathBuf};

use crate::mount::ItemMount;
use crate::account_folder::{SyncError, SyncService};

/// Why `Dehydrate` of `path`, which a pin holds for, is refused. `konedrivectl` reads the
/// path back out of exactly this shape.
pub fn refusal(path: &Path) -> String {
    format!("{} is pinned: unpin it first", path.display())
}

impl SyncService {
    /// `Pin(paths)`, "Always keep on this device": a pin holds for each path — a file, a
    /// folder, or the folder itself — from now on, and every file under it that is not
    /// downloaded is downloaded; how many paths this call changed a mark for.
    ///
    /// A folder is pinned whole: an exception or a pin below it comes off. A path a folder
    /// above it pins gets no mark of its own, and loses the exception it had. The mark is
    /// written first and the downloads follow, so a crash in between loses nothing: the
    /// next sweep finds them. Every path is checked before any is pinned: one outside the
    /// folder, or one OneDrive has no item for, refuses the call.
    pub async fn pin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        self.check_pinnable(paths).await?;
        self.set_pins(paths, true).await
    }

    /// What `Pin`, `Unpin` and `FreeUp` check of every path before they change anything,
    /// on its own: each path is an item of the folder. `Files` asks every account whose
    /// folder a call's paths are in first, so that a call that spans accounts is refused
    /// as a whole or not at all.
    pub async fn check_pinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        for path in paths {
            mount.files_interface().find_path_status(path).await?;
        }
        Ok(())
    }

    /// `Unpin(paths)`, unchecking "Always keep on this device": no pin holds for each path
    /// from now on, and nothing else changes — its files stay downloaded. How many paths
    /// this call changed a mark for. A path a folder above it pins gets an exception: the
    /// folder stays pinned, and the path and what is under it are not kept. A folder is
    /// let go whole: a pin or an exception below it comes off. Every path is checked
    /// before any mark changes.
    pub async fn unpin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        self.check_pinnable(paths).await?;
        self.set_pins(paths, false).await
    }

    /// Makes a pin hold for each path, or not; how many paths a mark changed for.
    pub(super) async fn set_pins(&self, paths: &[PathBuf], pinned: bool) -> Result<u32, SyncError> {
        let mount = self.require_mount()?;
        let mut changed = 0;
        let mut refused = None;
        for path in paths {
            match mount.files_interface().set_pin(path, pinned).await {
                Ok(true) => changed += 1,
                Ok(false) => {}
                Err(refusal) => {
                    refused = Some(refusal);
                    break;
                }
            }
        }
        // Also when a path was refused: the marks changed before it stay changed, and `PinnedCount` says so.
        self.show_pins(&mount).await;
        match refused {
            Some(refusal) => Err(refusal.into()),
            None => Ok(changed),
        }
    }

    /// `PinnedCount` and `PinnedBytes`, as the mount has them: written into the published
    /// state. Called after a start of the folder's sync and after every change of a pin
    /// made through the bus; what a listing changes of the pins is published by the plan's
    /// task with its round.
    pub(super) async fn show_pins(&self, mount: &ItemMount) {
        let pinned = mount.files_interface().count_pins().await;
        self.state.update_if_changed(|s| {
            s.local.pinned_count = u32::try_from(pinned.count).unwrap_or(u32::MAX);
            s.local.pinned_bytes = pinned.bytes;
        });
    }
}

#[cfg(test)]
mod tests;
