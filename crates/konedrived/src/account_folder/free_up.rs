//! Freeing up space: `Dehydrate`, `FreeUpSpace` and `FreeUp`, answered by the folder's mount,
//! which removes the downloaded copies; the files stay in the folder as names.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use std::path::{Path, PathBuf};

use crate::mount::{FreeUpDecision, ItemPathStatus};
use crate::status::activity::{self, Kind};
use crate::account_folder::pins::refusal;
use crate::account_folder::{SyncError, SyncService};

/// The refusal of a path a pin keeps: it names the folder above that pins it, or the path
/// itself for a pin of its own.
fn pinned(path: &Path, status: &ItemPathStatus) -> SyncError {
    // TODO: `ItemPathStatus::pin_holder`: `PinHolder::DirectoryAbove { path }` names the folder above, `PinHolder::Itself` the path.
    todo!()
}

/// What `FreeUp` refuses the path `path` for, by what a free-up decides for it once its own
/// pin is off: a folder above that pins it (`NotAllowed`), and a change of it that waits to
/// be uploaded, which freeing it up would lose (`NotUploaded`). Anything else is left to the
/// free-up itself, which counts what it leaves.
fn refuse_free_up(path: &Path, status: &ItemPathStatus) -> Result<(), SyncError> {
    // TODO: by the free-up's decision once the path's own pin is off, which the caller asked the mount for
    // (`ItemFilesInterface::decide_free_up(path, true)`) and gives here beside the status — the signature gains
    // that `FreeUpDecision` when this is filled.
    todo!()
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
        // TODO: `ItemFilesInterface::decide_free_up(path, false)` said as the refusal (`find_path_status` for the pin's holder), then `free_up`.
        todo!()
    }

    /// `FreeUpSpace()`: every downloaded file of the folder is freed. One a pin keeps on
    /// this device, a download is filling, or that holds a change not uploaded yet is left
    /// as it is and counted in [`FreedUp::pinned`]. What was freed is one `freed` event for
    /// the folder, not one per file.
    pub async fn free_up_space(&self) -> Result<FreedUp, SyncError> {
        // TODO: `ItemFilesInterface::free_up_all` (the mount's `item_files_interface`).
        todo!()
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_unpinnable`](Self::check_unpinnable)'s rules, and no file named itself has a
    /// change waiting to be uploaded (`NotUploaded`). `Files` asks every account whose
    /// folder a call's paths are in first, so that a call that spans accounts is refused as
    /// a whole or not at all.
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        // TODO: after `check_unpinnable`, for each path `ItemFilesInterface::decide_free_up(path, true)`: `refuse_free_up`.
        todo!()
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
        // TODO: `ItemFilesInterface::find_path_status` (`pin_holder`: a path pinned from above is refused, one
        // with its own pin loses it by `set_pin`), then `free_up`, then `show_pins`.
        todo!()
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
