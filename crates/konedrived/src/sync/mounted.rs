//! What the folder is asked for, answered by its mount: a folder served through FUSE
//! (`crate::fuse`) has no placeholder to open, lease or punch, and no helper to ask.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::State;

use crate::fuse::{Mount, Refused};
use crate::hydration::pin;
use crate::status::activity::{self, Kind};
use crate::sync::free_up::{freed_detail, FreedUp};
use crate::sync::menu::{FreeUpWhy, Part, Taken};
use crate::sync::{SyncError, SyncService};

impl From<Refused> for SyncError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::NotOurs => SyncError::OutsideRoot,
            Refused::Pinned { .. } => SyncError::NotAllowed(refused.to_string()),
            Refused::Io(why) => SyncError::Io(why),
        }
    }
}

/// `path` is kept on this device by a pin: its own (`above` false) or a folder's above it.
fn kept_by_a_pin(mount: &Mount, path: &Path, own_counts: bool) -> Result<bool, SyncError> {
    let looked = mount.look(path)?;
    if let Some(by) = &looked.pinned_above {
        return Err(SyncError::NotAllowed(pin::refusal(path, by)));
    }
    if looked.own_pin && own_counts {
        return Err(SyncError::NotAllowed(pin::refusal(path, path)));
    }
    Ok(looked.own_pin)
}

impl SyncService {
    /// `Dehydrate`: one file freed, refused for what a pin keeps.
    pub(super) async fn mounted_dehydrate(&self, mount: &Arc<Mount>, path: &Path) -> Result<(), SyncError> {
        kept_by_a_pin(mount, path, true)?;
        if mount.look(path)?.state != Some(State::Hydrated) {
            return Err(SyncError::NotHydrated);
        }
        let freed = mount.free_up(path).await?;
        if freed.files == 0 {
            return Err(SyncError::InUse);
        }
        self.report.activity.record(vec![activity::event(Kind::Freed, path.display().to_string(), activity::human_size(freed.bytes))]).await;
        Ok(())
    }

    /// `FreeUpSpace`: every downloaded file that no pin keeps.
    pub(super) async fn mounted_free_up_space(&self, mount: &Arc<Mount>) -> Result<FreedUp, SyncError> {
        let reg = self.require_record()?;
        let freed = mount.free_up_all().await;
        let total = FreedUp { files: freed.files as u32, bytes: freed.bytes, pinned: freed.kept as u32, ..FreedUp::default() };
        if total.files > 0 {
            let event = activity::event(Kind::Freed, reg.root.path.display().to_string(), freed_detail(total.files, total.bytes));
            self.report.activity.record(vec![event]).await;
        }
        Ok(total)
    }

    /// `FreeUp`: each path's own pin comes off, and what it holds is freed; refused as a
    /// whole when a folder above one of them pins it.
    pub(super) async fn mounted_free_up(&self, mount: &Arc<Mount>, paths: &[PathBuf]) -> Result<FreedUp, SyncError> {
        let mut own = Vec::new();
        for path in paths {
            if kept_by_a_pin(mount, path, false)? {
                own.push(path);
            }
        }
        for path in own {
            mount.set_pin(path, false).await?;
        }
        self.show_pins(mount);
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

    /// `Pin` and `Unpin`: how many pins were put on or taken off. A path a folder above
    /// pins already is left as it is by `Pin`, and refused by `Unpin`.
    pub(super) async fn mounted_pin(&self, mount: &Arc<Mount>, paths: &[PathBuf], pinned: bool) -> Result<u32, SyncError> {
        let mut changed = 0;
        for path in paths {
            let looked = mount.look(path)?;
            if let Some(by) = &looked.pinned_above {
                if pinned {
                    continue;
                }
                return Err(SyncError::NotAllowed(pin::refusal(path, by)));
            }
            if looked.own_pin != pinned {
                mount.set_pin(path, pinned).await?;
                changed += 1;
            }
        }
        self.show_pins(mount);
        Ok(changed)
    }

    /// `PinnedCount`, as the mount has it.
    pub(super) fn show_pins(&self, mount: &Mount) {
        let count = mount.pinned_count() as u32;
        self.state.update(|s| s.local.pinned_count = count);
    }
}

/// `Files.Menu`'s facts for `paths`, from the mount.
pub(super) fn menu_part(mount: &Mount, paths: &[PathBuf]) -> Part {
    let mut part = Part { taken: Vec::with_capacity(paths.len()), ..Part::default() };
    for path in paths {
        let Ok(looked) = mount.look(path) else {
            part.taken.push(None);
            continue;
        };
        if part.kept_by.is_none() {
            part.kept_by.clone_from(&looked.pinned_above);
        }
        part.taken.push(Some(Taken {
            is_dir: looked.folder,
            own: looked.own_pin,
            pinned: looked.own_pin || looked.pinned_above.is_some(),
            hydrated: looked.state == Some(State::Hydrated),
            in_onedrive: true,
        }));
    }
    part.free_up_refused = part.kept_by.is_some().then_some(FreeUpWhy::PinnedAbove);
    part
}
