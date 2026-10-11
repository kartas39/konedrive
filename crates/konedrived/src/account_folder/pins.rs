//! "Always keep on this device": pins put on and taken off. A pin is a row of the folder's
//! tree store, kept by its mount, which downloads what a pin keeps.

use std::path::{Path, PathBuf};

use crate::mount::{ItemMount, PinHolder};
use crate::account_folder::{SyncError, SyncService};

/// Why a free-up or unpin of `path`, which `by` pins, is refused: the path refused and what
/// pins it — `by` is `path` itself for `Dehydrate` of a file with a pin of its own.
/// `konedrivectl` reads both back out of exactly this shape; the Dolphin plugin shows it as
/// it is.
pub fn refusal(path: &Path, by: &Path) -> String {
    format!("{} is pinned by {}: unpin it first", path.display(), by.display())
}

impl SyncService {
    /// `Pin(paths)`, "Always keep on this device": each path — a file, a folder, or the
    /// folder itself — gets a pin, and every file under it that is not downloaded is
    /// downloaded; how many pins were put on by this call.
    ///
    /// The pin is written first and the downloads follow, so a crash in between loses
    /// nothing: the next sweep finds them. A path a folder above it pins already is left as
    /// it is. Every path is checked before any is pinned: one outside the folder, or one
    /// OneDrive has no item for, refuses the call.
    pub async fn pin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        self.check_pinnable(paths).await?;
        self.set_pins(paths, true).await
    }

    /// What `Unpin` and `FreeUp` check of every path before they change anything, on its
    /// own: each path is an item of the folder, and no folder above it pins it
    /// (`NotAllowed`). `Files` asks every account whose folder a call's paths are in first,
    /// so that a call that spans accounts is refused as a whole or not at all.
    pub async fn check_unpinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        for path in paths {
            if let PinHolder::DirectoryAbove { path: by } = mount.files_interface().find_path_status(path).await?.pin_holder {
                return Err(SyncError::NotAllowed(refusal(path, &by)));
            }
        }
        Ok(())
    }

    /// What `Pin` checks of every path before it pins any, on its own: each path is an item
    /// of the folder. `Files` asks every account first, as for
    /// [`check_unpinnable`](Self::check_unpinnable).
    pub async fn check_pinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        for path in paths {
            mount.files_interface().find_path_status(path).await?;
        }
        Ok(())
    }

    /// `Unpin(paths)`, unchecking "Always keep on this device": each path's own pin comes
    /// off, and nothing else changes — its files stay downloaded. How many pins came off. A
    /// path a folder above it pins is refused `NotAllowed`, naming the folder, as `FreeUp`
    /// refuses it; every path is checked before any pin comes off.
    pub async fn unpin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        self.check_unpinnable(paths).await?;
        self.set_pins(paths, false).await
    }

    /// Puts each path's own pin on, or takes it off; how many changed. A path a folder
    /// above it pins is left as it is.
    async fn set_pins(&self, paths: &[PathBuf], pinned: bool) -> Result<u32, SyncError> {
        let mount = self.require_mount()?;
        let mut changed = 0;
        let mut refused = None;
        for path in paths {
            // A path a folder above it pins is left as it is.
            let set = match mount.files_interface().find_path_status(path).await {
                Ok(status) if matches!(status.pin_holder, PinHolder::DirectoryAbove { .. }) => continue,
                Ok(_) => mount.files_interface().set_pin(path, pinned).await,
                Err(refusal) => Err(refusal),
            };
            match set {
                Ok(true) => changed += 1,
                Ok(false) => {}
                Err(refusal) => {
                    refused = Some(refusal);
                    break;
                }
            }
        }
        // Also when a path was refused: the pins changed before it stay changed, and `PinnedCount` says so.
        self.show_pins(&mount).await;
        match refused {
            Some(refusal) => Err(refusal.into()),
            None => Ok(changed),
        }
    }

    /// `PinnedCount`, as the mount has it: written into the published state. Called after
    /// a start of the folder's sync and after every change of a pin made through the bus;
    /// what a listing changes of the pins is published by the plan's task with its round.
    pub(super) async fn show_pins(&self, mount: &ItemMount) {
        let count = u32::try_from(mount.files_interface().count_pins().await).unwrap_or(u32::MAX);
        self.state.update_if_changed(|s| s.local.pinned_count = count);
    }
}
