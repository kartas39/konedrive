use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::folder::root::SyncRoot;
use crate::hydration::source::{Answered, FillError};
use crate::helper::NotCleared;
use crate::folder::locks::InodeKey;
use crate::sync::{SyncError, SyncService};
use crate::hydration::pin;

impl SyncService {
    /// Puts a pin on each of `targets`, or takes it off ([`pin::set_pin`]), stopping at the
    /// first failure: the paths done, and that failure. A file's pin is
    /// written under its per-inode lock, since a fill lifts the same write
    /// bit around its own attribute writes; each descriptor is closed once
    /// its write is done.
    pub(super) async fn set_pins(&self, targets: Vec<PinTarget>, on: bool) -> (Vec<PathBuf>, Option<SyncError>) {
        let mut done = Vec::new();
        for PinTarget { item, shown, is_dir, .. } in targets {
            let guard = if is_dir {
                None
            } else {
                match InodeKey::of(&item) {
                    Ok(key) => Some(self.locks.lock(key).await),
                    Err(e) => return (done, Some(SyncError::Io(format!("{}: {e}", shown.display())))),
                }
            };
            let written = tokio::task::spawn_blocking(move || pin::set_pin(&item, on)).await;
            drop(guard);
            match written {
                Ok(Ok(())) => done.push(shown),
                Ok(Err(e)) => return (done, Some(SyncError::Io(format!("{}: {e}", shown.display())))),
                Err(e) => return (done, Some(SyncError::Io(format!("the pin task failed: {e}")))),
            }
        }
        (done, None)
    }

    /// Every one of `paths` opened and looked at ([`pin_targets`]), on a
    /// blocking thread.
    pub(super) async fn pin_targets(&self, root: &SyncRoot, paths: &[PathBuf]) -> Result<Vec<PinTarget>, SyncError> {
        let (root, paths) = (root.clone(), paths.to_vec());
        tokio::task::spawn_blocking(move || pin_targets(&root, &paths))
            .await
            .map_err(|e| SyncError::Io(format!("the pin task failed: {e}")))?
    }

    /// `Pin(paths)`, "Always keep on this device": each path — a file, a
    /// folder, or the folder itself — gets a pin, and every online-only file
    /// under it is queued for download ([`pin::Pins`]); how many were queued
    /// by this call.
    ///
    /// The pin is written first and the downloads follow, so a crash in
    /// between loses nothing: the next sweep finds them. A path a folder
    /// above it pins already is left as it is. Every path is checked before
    /// any is pinned: one outside the folder, a `.konedrive-*` name, or a
    /// file that is not ours refuses the call.
    pub async fn pin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        let reg = self.require_record()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        let (mut again, mut write) = (Vec::new(), Vec::new());
        for target in targets {
            if !target.above.is_empty() {
                continue;
            }
            if target.own {
                again.push(target.shown);
            } else {
                write.push(target);
            }
        }
        let (pinned, failed) = self.set_pins(write, true).await;
        for shown in &pinned {
            self.pins.pinned(shown.clone());
        }
        // What is pinned is queued, even when a later path could not be; a
        // path pinned already is looked through again.
        again.extend(pinned);
        let queued = self.pins.queue_under(again).await;
        match failed {
            Some(e) => Err(e),
            None => Ok(queued),
        }
    }

    /// What `Unpin` and `FreeUp` check of every path before they change
    /// anything, on its own: each path is in the folder and one of ours, and
    /// no folder above it pins it and stays pinned (`NotAllowed`). `Files`
    /// asks every account whose folder a call's paths are in first, so that
    /// a call that spans accounts is refused as a whole or not at all.
    pub async fn check_unpinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_record()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        match kept_by_folder(&targets) {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// What `Pin` checks of every path before it pins any, on its own: each
    /// path is in the folder, and one of ours. `Files` asks every account
    /// first, as for [`check_unpinnable`](Self::check_unpinnable).
    pub async fn check_pinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_record()?;
        self.pin_targets(&reg.root, paths).await.map(drop)
    }

    /// `Unpin(paths)`, unchecking "Always keep on this device": each path's
    /// own pin comes off, and nothing else changes — its files stay
    /// downloaded. How many pins came off. A path a folder above it pins is
    /// refused `NotAllowed`, naming the folder, as `FreeUp` refuses it
    /// ([`kept_by_folder`]); every path is checked before any pin comes off.
    pub async fn unpin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        let reg = self.require_record()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        if let Some(refusal) = kept_by_folder(&targets) {
            return Err(refusal);
        }
        let own: Vec<PinTarget> = targets.into_iter().filter(|target| target.own).collect();
        let (unpinned, failed) = self.set_pins(own, false).await;
        for shown in &unpinned {
            self.pins.unpinned(shown);
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(unpinned.len() as u32),
        }
    }

    /// `PinnedCount`.
    pub fn pinned_count(&self) -> u32 {
        self.state.get().local.pinned_count
    }
}

/// A path `Pin`, `Unpin` or `FreeUp` was given, opened beneath the root
/// (`SyncRoot::open_item`) and looked at, before anything changes.
pub(super) struct PinTarget {
    pub(super) item: File,
    /// Its full path as the activity log names it.
    pub(super) shown: PathBuf,
    pub(super) is_dir: bool,
    /// It carries a pin of its own.
    pub(super) own: bool,
    /// The folders above it, up to the root, that carry a pin: nearest first.
    above: Vec<PathBuf>,
}

/// Opens and looks at each of `paths`; one that cannot be — outside the
/// root, a `.konedrive-*` name, a file that is not ours — refuses them all.
/// Blocking.
pub(super) fn pin_targets(root: &SyncRoot, paths: &[PathBuf]) -> Result<Vec<PinTarget>, SyncError> {
    paths
        .iter()
        .map(|path| {
            let (item, shown) = root.open_item(path)?;
            let io = |e: io::Error| SyncError::Io(format!("{}: {e}", shown.display()));
            let is_dir = item.metadata().map_err(io)?.is_dir();
            let own = konedrive_fs::placeholder::read_pin(&item).map_err(io)?;
            let above = pin::pinned_ancestors(&root.path, &shown);
            Ok(PinTarget { item, shown, is_dir, own, above })
        })
        .collect()
}

/// Why pins cannot come off `targets`: a folder above one of them pins it
/// and stays pinned — it is not itself one of them with its own pin, which
/// the same call takes off. A path with a pin of its own under a pinned
/// folder is refused too: taking its pin off would leave it pinned.
pub(super) fn kept_by_folder(targets: &[PinTarget]) -> Option<SyncError> {
    let coming_off: std::collections::HashSet<&Path> =
        targets.iter().filter(|target| target.own).map(|target| target.shown.as_path()).collect();
    targets
        .iter()
        .flat_map(|target| target.above.iter().map(move |folder| (target, folder)))
        .find(|(_, folder)| !coming_off.contains(folder.as_path()))
        .map(|(target, folder)| SyncError::NotAllowed(pin::refusal(&target.shown, folder)))
}

/// A pinned download is an ordinary fill ([`SyncService::fill_now`]):
/// verified, checkpointed, shown in `Transfers` and recorded as
/// `downloaded` or `failed`. A file whose pin was taken off since it was
/// queued, or whose folder was forgotten, is passed over.
#[async_trait]
impl pin::PinFill for SyncService {
    async fn fill_pinned(&self, path: &Path) -> pin::Filled {
        let Some(reg) = self.record() else { return pin::Filled::Skipped };
        let (root, target) = (reg.root.path.clone(), path.to_path_buf());
        let still = tokio::task::spawn_blocking(move || pin::pinned_by(&root, &target).is_some())
            .await
            .unwrap_or(false);
        if !still {
            return pin::Filled::Skipped;
        }
        // The pins' worker holds a slot of the pool for it.
        match self.fill_now(path, None).await {
            Ok(Answered::Failed(FillError::Errno(errno))) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
                pin::Filled::NoSpace
            }
            // No link to the helper for a file that may carry an ignore mark.
            Ok(Answered::Failed(FillError::NotCleared(NotCleared::NoWay))) => {
                tracing::info!("{} is kept on this device but was not downloaded: {}", path.display(), SyncError::NoHelper);
                pin::Filled::Failed
            }
            Ok(Answered::Failed(_)) => pin::Filled::Failed,
            Ok(Answered::Filled) => pin::Filled::Done,
            // Found downloaded already: nothing was transferred.
            Ok(Answered::AlreadyThere | Answered::NotOurs) => pin::Filled::Skipped,
            Err(e) => {
                tracing::info!("{} is kept on this device but was not downloaded: {e}", path.display());
                pin::Filled::Failed
            }
        }
    }
}
