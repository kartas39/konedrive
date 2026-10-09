//! Freeing up space: `Dehydrate`, `FreeUpSpace` and `FreeUp`, answered by the folder's mount,
//! which removes the downloaded copies; the files stay in the folder as names.

use std::path::{Path, PathBuf};

use crate::fuse::{FreeUpDecision, PathStatus};
use crate::status::activity::{self, Kind};
use crate::sync::pins::refusal;
use crate::sync::{SyncError, SyncService};

/// The refusal of a path a pin keeps: it names the folder above that pins it, or the path
/// itself for a pin of its own.
fn pinned(path: &Path, status: &PathStatus) -> SyncError {
    SyncError::NotAllowed(refusal(path, status.pinned_above.as_deref().unwrap_or(path)))
}

/// What `FreeUp` refuses the path `path` for, by what a free-up decides for it once its own
/// pin is off: a folder above that pins it (`NotAllowed`), and a change of it that waits to
/// be uploaded, which freeing it up would lose (`NotUploaded`). Anything else is left to the
/// free-up itself, which counts what it leaves.
fn refuse_free_up(path: &Path, status: &PathStatus) -> Result<(), SyncError> {
    match status.decide_free_up_once_unpinned() {
        FreeUpDecision::LeavePinned => Err(pinned(path, status)),
        FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting => Err(SyncError::NotUploaded(path.display().to_string())),
        FreeUpDecision::Free | FreeUpDecision::LeaveDownloading | FreeUpDecision::NothingToFree => Ok(()),
    }
}

impl SyncService {
    /// Frees the space of one downloaded file. Recorded as a `freed` event with what it
    /// freed.
    ///
    /// Refused `NotAllowed` for a file a pin keeps on this device — its own, or a folder's
    /// above it: `FreeUp` takes a pin off. Refused `NotUploaded` for a file with a change
    /// waiting to be uploaded, `NotHydrated` for one that is not downloaded or is being
    /// downloaded, and `InUse` for one that could not be freed after all. The mount decides
    /// ([`FreeUpDecision`]); this says its decision as a refusal.
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        let status = mount.files_interface().find_path_status(path)?;
        match status.decide_free_up() {
            FreeUpDecision::Free => {}
            FreeUpDecision::LeavePinned => return Err(pinned(path, &status)),
            FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting => return Err(SyncError::NotUploaded(path.display().to_string())),
            FreeUpDecision::LeaveDownloading | FreeUpDecision::NothingToFree => return Err(SyncError::NotHydrated),
        }
        let freed = mount.files_interface().free_up(path).await?;
        if freed.files == 0 {
            return Err(SyncError::InUse);
        }
        self.report.activity.record(vec![activity::event(Kind::Freed, path.display().to_string(), activity::human_size(freed.bytes))]).await;
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
        }
        Ok(total)
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_unpinnable`](Self::check_unpinnable)'s rules, and no file named itself has a
    /// change waiting to be uploaded (`NotUploaded`). `Files` asks every account whose
    /// folder a call's paths are in first, so that a call that spans accounts is refused as
    /// a whole or not at all.
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        self.check_unpinnable(paths).await?;
        let mount = self.require_mount()?;
        paths.iter().try_for_each(|path| refuse_free_up(path, &mount.files_interface().find_path_status(path)?))
    }

    /// `FreeUp(paths)`, the menu's "Free up space":
    ///
    /// - a path with a pin of its own loses it, and then everything under it is freed up;
    /// - a path a folder above it pins is refused `NotAllowed`, naming that folder; checked
    ///   for every path before anything changes;
    /// - any other path is freed up.
    ///
    /// The pins come off first. One that cannot stops the call with that failure, before
    /// anything is freed; the pins already off stay off, and `PinnedCount` says so.
    ///
    /// A file below a path that a pin still keeps, a download is filling, or that holds a
    /// change not uploaded yet is left and counted in [`FreedUp::pinned`]. One `freed`
    /// event per path that freed anything.
    pub async fn free_up(&self, paths: &[PathBuf]) -> Result<FreedUp, SyncError> {
        let mount = self.require_mount()?;
        let mut own = Vec::new();
        for path in paths {
            let status = mount.files_interface().find_path_status(path)?;
            refuse_free_up(path, &status)?;
            if status.own_pin {
                own.push(path);
            }
        }
        for path in own {
            let unpinned = mount.files_interface().set_pin(path, false).await;
            self.show_pins(&mount);
            unpinned?;
        }
        let (mut total, mut events) = (FreedUp::default(), Vec::new());
        for path in paths {
            let freed = mount.files_interface().free_up(path).await?;
            total.files += freed.files as u32;
            total.bytes += freed.bytes;
            total.pinned += freed.kept as u32;
            if freed.files > 0 {
                events.push(activity::event(Kind::Freed, path.display().to_string(), freed_detail(freed.files as u32, freed.bytes)));
            }
        }
        if !events.is_empty() {
            self.report.activity.record(events).await;
        }
        Ok(total)
    }
}

/// What `FreeUpSpace()` and `FreeUp()` did: files freed up, the bytes of blocks that gave
/// back, and downloaded files left because something keeps them on this device (a pin, a
/// download filling them, a change not uploaded yet). `busy` and `modified` are always 0:
/// the bus still carries them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedUp {
    pub files: u32,
    pub bytes: u64,
    pub busy: u32,
    pub modified: u32,
    pub pinned: u32,
}

/// A `freed` event's detail for more than one file: "2 files, 1.5 MiB".
fn freed_detail(files: u32, bytes: u64) -> String {
    format!("{files} file{}, {}", if files == 1 { "" } else { "s" }, activity::human_size(bytes))
}
