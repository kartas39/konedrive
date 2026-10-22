//! Freeing up space: `Dehydrate`, `FreeUpSpace` and `FreeUp`, answered by the folder's mount,
//! which removes the downloaded copies; the files stay in the folder as names. What frees
//! anything has `LocalBytes` measured again.

use std::path::{Path, PathBuf};

use crate::mount::FreeUpDecision;
use crate::status::activity::{self, Kind};
use crate::account_folder::pins::refusal;
use crate::account_folder::{SyncError, SyncService};

/// What `FreeUp` refuses the path `path` for, by what a free-up decides for it once no pin
/// holds for it (`decision`: `ItemFilesInterface::decide_free_up(path, true)`): a change of
/// it that waits to be uploaded, which freeing it up would lose (`NotUploaded`). Anything
/// else is left to the free-up itself, which counts what it leaves.
fn refuse_free_up(path: &Path, decision: FreeUpDecision) -> Result<(), SyncError> {
    match decision {
        FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting => Err(SyncError::NotUploaded(path.display().to_string())),
        FreeUpDecision::Free | FreeUpDecision::LeavePinned | FreeUpDecision::LeaveDownloading | FreeUpDecision::NothingToFree => Ok(()),
    }
}

impl SyncService {
    /// Frees the space of one downloaded file. Recorded as a `freed` event with what it
    /// freed. A file that is not on disk is already what was asked for: answered at once,
    /// with no event.
    ///
    /// Refused `NotAllowed` for a file a pin keeps on this device: `FreeUp` is what lets
    /// such a file go. Refused `NotUploaded` for a file with a change
    /// waiting to be uploaded, `NotHydrated` for one that is being downloaded, and `InUse`
    /// for one that could not be freed after all. The mount decides ([`FreeUpDecision`]);
    /// this says its decision as a refusal.
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        let files = mount.files_interface();
        match files.decide_free_up(path, false).await? {
            FreeUpDecision::Free => {}
            FreeUpDecision::LeavePinned => return Err(SyncError::NotAllowed(refusal(path))),
            FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting => return Err(SyncError::NotUploaded(path.display().to_string())),
            FreeUpDecision::NothingToFree => return Ok(()),
            FreeUpDecision::LeaveDownloading => return Err(SyncError::NotHydrated),
        }
        let freed = files.free_up(path).await?;
        if freed.files == 0 {
            return Err(SyncError::InUse);
        }
        self.report.activity.record(vec![activity::event(Kind::Freed, path.display().to_string(), activity::human_size(freed.bytes))]).await;
        self.report.space.kick();
        Ok(())
    }

    /// `FreeUpSpace()`: every downloaded file of the folder is freed. One a pin keeps on
    /// this device, a download is filling, or that holds a change not uploaded yet is left
    /// as it is and counted in [`FreedUp::pinned`]. What was freed is one `freed` event for
    /// the folder, not one per file.
    pub async fn free_up_space(&self) -> Result<FreedUp, SyncError> {
        let reg = self.require_record()?;
        let freed = self.require_mount()?.files_interface().free_up_all().await?;
        let total = FreedUp { files: freed.files as u32, bytes: freed.bytes, pinned: freed.kept as u32, ..FreedUp::default() };
        if total.files > 0 {
            let event = activity::event(Kind::Freed, reg.root.path.display().to_string(), freed_detail(total.files, total.bytes));
            self.report.activity.record(vec![event]).await;
            self.report.space.kick();
        }
        Ok(total)
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_pinnable`](Self::check_pinnable)'s rule, and no file named itself has a
    /// change waiting to be uploaded (`NotUploaded`). `Files` asks every account whose
    /// folder a call's paths are in first, so that a call that spans accounts is refused as
    /// a whole or not at all.
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        let files = mount.files_interface();
        for path in paths {
            refuse_free_up(path, files.decide_free_up(path, true).await?)?;
        }
        Ok(())
    }

    /// `FreeUp(paths)`, the menu's "Free up space": no pin holds for each path from now
    /// on, as after [`unpin`](Self::unpin) — a path a folder above it pins gets an
    /// exception, and a folder is let go whole — and then everything under it is freed up.
    ///
    /// Every path is checked before anything changes. The marks are written first; one
    /// that cannot be stops the call with that failure, before anything is freed; the
    /// marks already written stay, and `PinnedCount` says so.
    ///
    /// A file below a path that a download is filling, or that holds a change not uploaded
    /// yet, is left and counted in [`FreedUp::pinned`]. One `freed` event per path that
    /// freed anything.
    pub async fn free_up(&self, paths: &[PathBuf]) -> Result<FreedUp, SyncError> {
        self.check_free_up(paths).await?;
        self.set_pins(paths, false).await?;
        let mount = self.require_mount()?;
        let files = mount.files_interface();
        let (mut total, mut events) = (FreedUp::default(), Vec::new());
        for path in paths {
            let freed = files.free_up(path).await?;
            total.files += freed.files as u32;
            total.bytes += freed.bytes;
            total.pinned += freed.kept as u32;
            if freed.files > 0 {
                events.push(activity::event(Kind::Freed, path.display().to_string(), freed_detail(freed.files as u32, freed.bytes)));
            }
        }
        if !events.is_empty() {
            self.report.activity.record(events).await;
            self.report.space.kick();
        }
        Ok(total)
    }
}

/// What `FreeUpSpace()` and `FreeUp()` did: files freed up, the bytes of blocks that gave
/// back, and downloaded files left because something keeps them on this device (a pin, a
/// download filling them, a change not uploaded yet).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedUp {
    pub(crate) files: u32,
    pub(crate) bytes: u64,
    pub(crate) pinned: u32,
}

/// A `freed` event's detail for more than one file: "2 files, 1.5 MiB".
fn freed_detail(files: u32, bytes: u64) -> String {
    format!("{files} file{}, {}", if files == 1 { "" } else { "s" }, activity::human_size(bytes))
}

#[cfg(test)]
mod tests;
