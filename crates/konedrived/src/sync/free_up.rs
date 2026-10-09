//! Freeing up space: `Dehydrate`, `FreeUpSpace` and `FreeUp`, answered by the folder's mount,
//! which removes the downloaded copies; the files stay in the folder as names.

use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::State;

use crate::fuse::Mount;
use crate::status::activity::{self, Kind};
use crate::sync::pins::refusal;
use crate::sync::{SyncError, SyncService};

/// Whether `path` carries a pin of its own. Refused when a folder above it pins it, and,
/// where `own_refuses`, when it carries one itself.
fn own_pin(mount: &Mount, path: &Path, own_refuses: bool) -> Result<bool, SyncError> {
    let status = mount.find_path_status(path)?;
    if let Some(by) = &status.pinned_above {
        return Err(SyncError::NotAllowed(refusal(path, by)));
    }
    if status.own_pin && own_refuses {
        return Err(SyncError::NotAllowed(refusal(path, path)));
    }
    Ok(status.own_pin)
}

/// Refuses a free-up of the file at `path` while a change of it waits to be uploaded:
/// freeing it up would lose that change (`NotUploaded`).
fn refuse_unuploaded(mount: &Mount, path: &Path) -> Result<(), SyncError> {
    if mount.find_path_status(path)?.modified_here {
        return Err(SyncError::NotUploaded(path.display().to_string()));
    }
    Ok(())
}

impl SyncService {
    /// Frees the space of one downloaded file. Recorded as a `freed` event with what it
    /// freed.
    ///
    /// Refused `NotAllowed` for a file a pin keeps on this device — its own, or a folder's
    /// above it: `FreeUp` takes a pin off. Refused `NotUploaded` for a file with a change
    /// waiting to be uploaded, `NotHydrated` for one that is not downloaded, and `InUse`
    /// for one a download is filling.
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        let mount = self.require_mount()?;
        own_pin(&mount, path, true)?;
        refuse_unuploaded(&mount, path)?;
        if mount.find_path_status(path)?.state != Some(State::Hydrated) {
            return Err(SyncError::NotHydrated);
        }
        let freed = mount.free_up(path).await?;
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
        let freed = self.require_mount()?.free_up_all().await;
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
        paths.iter().try_for_each(|path| refuse_unuploaded(&mount, path))
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
            if own_pin(&mount, path, false)? {
                own.push(path);
            }
            refuse_unuploaded(&mount, path)?;
        }
        for path in own {
            let unpinned = mount.set_pin(path, false).await;
            self.show_pins(&mount);
            unpinned?;
        }
        let (mut total, mut events) = (FreedUp::default(), Vec::new());
        for path in paths {
            let freed = mount.free_up(path).await?;
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
