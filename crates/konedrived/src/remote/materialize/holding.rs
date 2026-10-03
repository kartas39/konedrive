use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::folder::disk::{Probe, Scanned, HOLDING};
use crate::status::activity::Kind as EventKind;
use konedrive_tree::Table;
use super::{holds_local_work, rw, survey_stopped, ApplyError, Materializer, Rescued, Run};

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
        // What is leaving inside it goes along (issue #104).
        let (from, to) = (rel.to_path_buf(), PathBuf::from(HOLDING).join(id));
        self.store.call_blocking(move |s| s.leaving_rebase(&from, &to))?;
        Ok(())
    }

    pub(super) fn drain_holding(&self, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let shown = name
                .to_str()
                .and_then(|id| run.moved_from.get(id).cloned())
                .unwrap_or_else(|| PathBuf::from(HOLDING).join(&name));
            // What goes is forgotten first, and its downloads stop (issue
            // #104); what is rescued keeps its content, out of the folder.
            let survey = self.forget_before_removing(&holding, &name, true)?;
            let deleted = run.out.deleted;
            run.stopped = survey_stopped(&survey);
            let result = self.delete_tree(&holding, &name, &shown, run);
            if result.is_err() {
                self.settle_stopped(&holding, &name, &survey);
            }
            result?;
            // One event for what went, however much was inside it; what was
            // rescued instead is a conflict, not a removal.
            if run.out.deleted > deleted {
                run.note(EventKind::Removed, &shown, None);
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.remove(&root, OsStr::new(HOLDING), true)?;
        Ok(())
    }

    /// Deletes what the cloud no longer has, rescuing anything that would
    /// lose a local byte.
    fn delete_tree(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        match self.disk.probe(dir, name)? {
            Probe::Absent => Ok(()),
            Probe::Unmanaged { .. } => self.rescue(dir, name, shown, run),
            Probe::Managed { id, .. } if self.claimed_elsewhere(&id)? => {
                // It survives, out of the folder: a download stopped in it is
                // a placeholder again first (issue #104).
                if !run.stopped.is_empty() {
                    let survey = rw::Survey::stopped_only(run.stopped.clone());
                    self.settle_stopped(dir, name, &survey);
                }
                self.set_aside(dir, name, shown, run)
            }
            Probe::Managed { is_dir: true, .. } => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.delete_tree(&sub, &child, &shown.join(&child), run)?;
                }
                self.disk.remove(dir, name, true)?;
                run.out.deleted += 1;
                Ok(())
            }
            Probe::Managed { is_dir: false, .. } => {
                let file = self.disk.open_file(dir, name)?;
                if holds_local_work(&file) {
                    self.rescue(dir, name, shown, run)
                } else {
                    self.disk.remove(dir, name, false)?;
                    run.out.deleted += 1;
                    Ok(())
                }
            }
        }
    }

    pub(super) fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.rescue(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} held local work the cloud's change would have lost; it is kept at {}",
            shown.display(),
            dest.display()
        );
        run.out.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
        Ok(())
    }

    /// Whether `id`, which is about to be removed, is another account's: one
    /// this folder's tree does not know, and another account claims.
    fn claimed_elsewhere(&self, id: &str) -> Result<bool, ApplyError> {
        let Some(claimed) = &self.claimed else { return Ok(false) };
        let known = self.store.call_blocking({ let id = id.to_owned(); move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some()) })?;
        Ok(!known && claimed(id))
    }

    /// Another account's object, moved here from its folder (write design
    /// §8.3): moved out of the folder like a rescue, but alive, attributes
    /// and all, so that the other account's move out finds it by its handle
    /// and downloads it where it is now. Never removed: that account's
    /// OneDrive may be the only other place its content is.
    fn set_aside(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.set_aside(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} is another account's, moved here from its folder; it is kept at {}, where that account downloads it",
            shown.display(),
            dest.display()
        );
        run.out.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
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
