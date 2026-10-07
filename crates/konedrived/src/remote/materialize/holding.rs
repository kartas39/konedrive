use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::folder::disk::{Probe, Scanned, HOLDING};
use crate::status::activity::Kind as EventKind;
use konedrive_tree::Table;
use super::removal::Policy;
use super::{holds_local_work, ApplyError, Materializer, Rescued, Run};

impl Materializer {
    pub(super) fn holding_if_any(&self) -> Result<Option<File>, ApplyError> {
        let root = self.disk.dir(Path::new(""))?;
        match self.disk.probe(&root, OsStr::new(HOLDING))? {
            Probe::Absent => Ok(None),
            Probe::Unmanaged { is_dir: true } => Ok(Some(self.disk.dir(Path::new(HOLDING))?)),
            other => Err(ApplyError::Io(format!("{HOLDING} is {other:?}, not the daemon's holding directory"))),
        }
    }

    fn holding(&self, run: &mut Run) -> Result<File, ApplyError> {
        let holding = match self.holding_if_any()? {
            Some(holding) => holding,
            None => {
                let root = self.disk.dir(Path::new(""))?;
                self.disk.make_dir(&root, OsStr::new(HOLDING))?
            }
        };
        // Files wait here; an open of one must still be intercepted — also in
        // a holding directory left by a cycle whose marking failed.
        if !run.holding_marked {
            self.mark(&holding, Path::new(HOLDING))?;
            run.holding_marked = true;
        }
        Ok(holding)
    }

    pub(super) fn to_holding(&self, rel: &Path, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        if parent == Path::new(HOLDING) && name == OsStr::new(id) {
            return Ok(());
        }
        let holding = self.holding(run)?;
        let dir = self.disk.dir(parent)?;
        self.disk.rename(&dir, name, &holding, OsStr::new(id))?;
        run.moved_from.entry(id.to_owned()).or_insert_with(|| rel.to_path_buf());
        // Read-write mode: the rows of what waits below a folder travel with
        // it, each folder's with its own: by the path it has in the holding
        // directory, which no other folder has, and from there to where it
        // is placed or put back.
        self.rows_follow(rel.to_path_buf(), PathBuf::from(HOLDING).join(id))
    }

    pub(super) fn drain_rescuing(&self, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let shown = name
                .to_str()
                .and_then(|id| run.moved_from.get(id).cloned())
                .unwrap_or_else(|| PathBuf::from(HOLDING).join(&name));
            // What is rescued instead keeps its content, out of the folder.
            let deleted = run.out.counts.deleted;
            self.take_off(&holding, &name, &shown, Policy::Removed, run)?;
            // One event for what went, however much was inside it; what was
            // rescued instead is a conflict, not a removal.
            if run.out.counts.deleted > deleted {
                run.note(EventKind::Removed, &shown, None);
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.remove(&root, OsStr::new(HOLDING), true)?;
        Ok(())
    }

    pub(super) fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let Some(dest) = self.disk.rescue(dir, name, shown, &self.rescue_into)? else {
            // A placeholder: nothing of the user's in it, so nothing to keep.
            tracing::info!("{} held nothing made here; it is removed, not rescued", shown.display());
            return Ok(());
        };
        tracing::warn!(
            "{} held local work the cloud's change would have lost; it is kept at {}",
            shown.display(),
            dest.display()
        );
        run.out.on_disk.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
        Ok(())
    }

    /// Whether `id`, which is about to be removed, is another account's: one
    /// this folder's tree does not know, and another account claims.
    pub(super) fn claimed_elsewhere(&self, id: &str) -> Result<bool, ApplyError> {
        let Some(claimed) = &self.claimed else { return Ok(false) };
        let known = self.store.call_blocking({ let id = id.to_owned(); move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some()) })?;
        Ok(!known && claimed(id))
    }

    /// Another account's object, moved here from its folder (`docs/design/writes.md`
    /// §8.3): moved out of the folder like a rescue, but alive, attributes
    /// and all, so that the other account's move out finds it by its handle
    /// and downloads it where it is now. Never removed: that account's
    /// OneDrive may be the only other place its content is.
    pub(super) fn set_aside(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.set_aside(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} is another account's, moved here from its folder; it is kept at {}, where that account downloads it",
            shown.display(),
            dest.display()
        );
        run.out.on_disk.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
        Ok(())
    }

    /// A name of the form `.konedrive-new-<id>` whose id
    /// also turns up somewhere else in the scan is not a folder waiting to be
    /// placed (that case is one entry per id, and is left to the usual
    /// misplaced/holding path) — it is the temporary link `swap_in` leaves
    /// when a crash, or a rename that fails after its `linkat` already
    /// succeeded, keeps a downloaded replacement from ever landing on the old
    /// name. Sending it to holding would collide with the real entry under
    /// the same id; it is discarded instead, like anything else the cloud
    /// does not know about, rescued first if it holds local work nobody made.
    pub(super) fn discard_leftover_replacement(&self, entry: &Scanned, run: &mut Run) -> Result<(), ApplyError> {
        let parent = entry.rel.parent().unwrap_or(Path::new(""));
        let name = entry.rel.file_name().expect("is_leftover_replacement checked this");
        let dir = self.disk.dir(parent)?;
        let work = self.disk.open_file(&dir, name).map(|f| holds_local_work(&f)).unwrap_or(false);
        if work {
            self.rescue(&dir, name, &entry.rel, run)
        } else {
            self.disk.remove(&dir, name, false)?;
            Ok(())
        }
    }
}
