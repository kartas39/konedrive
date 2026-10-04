use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::remote::materialize::{ApplyError, Materializer, Run};
use crate::folder::disk::{Probe, HOLDING};
use crate::local::names::copy_name;
use konedrive_tree::Table;
use crate::remote::materialize::removal::Removal;
use super::{is_new_name, Rw};

impl Materializer {
    /// What is left in the holding directory — what this run moved there and
    /// could not place (a local change holds its new folder), or what a stop
    /// or a crash left: what OneDrive removed goes as
    /// [`Self::take_off`] says; the rest goes back into the folder
    /// ([`Self::put_back`]). Nothing leaves the folder.
    pub(in crate::remote::materialize) fn drain_holding_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let id = name.to_str().map(str::to_owned);
            let placed = match &id {
                Some(id) => self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed),
                None => false,
            };
            if !placed && self.finish_new_folder(&holding, &name, id.as_deref(), run)? {
                continue;
            }
            // Removed in OneDrive: it goes (issue #104). One that is still
            // there but no longer placed goes back, and leaves from there.
            let removed = match &id {
                Some(id) => {
                    matches!(self.disk.probe(&holding, &name)?, Probe::Managed { id: ref there, .. } if there == id)
                        && self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?.is_none()
                }
                None => false,
            };
            let held = Path::new(HOLDING).join(&name);
            if !placed && removed {
                if self.take_off(&holding, &name, &held, rw.removed(), run)?.removal == Removal::Gone {
                    continue;
                }
                // What stays of it goes back where it was: said under that path.
                if let Some(was) = id.as_deref().and_then(|id| run.moved_from.get(id).cloned()) {
                    if let Some(at) = run.out.on_disk.kept.iter().position(|(rel, _)| *rel == held) {
                        let (_, kept) = run.out.on_disk.kept.remove(at);
                        run.out.on_disk.note_kept(&was, kept);
                    }
                }
            }
            if let Some(id) = &id {
                if placed {
                    run.out.pending.unsettled.insert(id.clone());
                }
            }
            let back = id.as_deref().and_then(|id| run.moved_from.get(id).cloned());
            self.put_back(&holding, &name, id.as_deref(), back, run)?;
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.remove(&root, OsStr::new(HOLDING), true)?;
        Ok(())
    }

    /// A new folder a stop left under its temporary name (`.konedrive-new-<id>`)
    /// whose item neither the base nor the tree has any more — OneDrive removed
    /// it meanwhile: it was only ever the daemon's, so it goes, and never comes
    /// back under its id as a name. Anything someone put in it is
    /// put back where the folder stood, each under its own name. Whether it was
    /// such a folder.
    fn finish_new_folder(&self, holding: &File, name: &OsStr, id: Option<&str>, run: &mut Run) -> Result<bool, ApplyError> {
        let Some(id) = id else { return Ok(false) };
        if !matches!(self.disk.probe(holding, name)?, Probe::Managed { is_dir: true, .. }) {
            return Ok(false);
        }
        let from = run.moved_from.get(id).cloned();
        if from.as_ref().is_some_and(|f| !is_new_name(f)) || self.store.call_blocking({ let id = id.to_owned(); move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some()) })? {
            return Ok(false);
        }
        let parent = from.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf).unwrap_or_default();
        let sub = self.disk.open_subdir(holding, name)?;
        for child in self.disk.list(&sub)? {
            if !self.put_at(&sub, &child, &parent.join(&child), run)? {
                self.put_at(&sub, &child, Path::new(&child), run)?;
            }
        }
        self.disk.remove(holding, name, true)?;
        Ok(true)
    }

    /// Takes `name` out of the holding directory, back into the folder: to
    /// `back`, where this run took it from; else where the base has item
    /// `id`; else where the tree has it; each beside its name under a copy
    /// name when that is taken now; and last, under a copy name in the root.
    /// Never out of the folder: whatever is out of it is a move out, and
    /// deleted in OneDrive.
    fn put_back(&self, holding: &File, name: &OsStr, id: Option<&str>, back: Option<PathBuf>, run: &mut Run) -> Result<(), ApplyError> {
        let mut places: Vec<PathBuf> = back.into_iter().filter(|b| !is_new_name(b)).collect();
        if let Some(id) = id {
            for table in [Table::Items, Table::Staging] {
                if let Some(at) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(table, &id) })?.filter(|l| l.placed && !l.rel.as_os_str().is_empty()) {
                    places.push(at.rel);
                }
            }
        }
        let fallback = places.first().and_then(|p| p.file_name()).map(|n| n.to_owned()).unwrap_or_else(|| name.to_owned());
        places.push(PathBuf::from(fallback));
        for place in &places {
            if self.put_at(holding, name, place, run)? {
                return Ok(());
            }
        }
        Err(ApplyError::Io(format!("{} could not be put back into the folder", PathBuf::from(HOLDING).join(name).display())))
    }

    /// Renames `name` from the holding directory to `place`, or beside it
    /// under the first free copy name. Whether it went: `false` when
    /// `place`'s folder is not there.
    fn put_at(&self, holding: &File, name: &OsStr, place: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        let parent = place.parent().unwrap_or(Path::new(""));
        let Some(to) = place.file_name() else { return Ok(false) };
        let Ok(dir) = self.disk.dir(parent) else { return Ok(false) };
        let machine = self.rw.as_ref().map(|rw| rw.machine.clone()).unwrap_or_default();
        let mut candidates = vec![to.to_os_string()];
        if let Some(wanted) = to.to_str() {
            candidates.extend((1..=100).map(|n| copy_name(wanted, &machine, n).into()));
        }
        for candidate in candidates {
            match self.disk.rename(holding, name, &dir, &candidate) {
                Ok(()) => {
                    // What is leaving inside it went along (issue #104).
                    let (from, to) = (PathBuf::from(HOLDING).join(name), parent.join(&candidate));
                    self.store.call_blocking(move |s| s.leaving_rebase(&from, &to))?;
                    let is_dir = matches!(self.disk.probe(&dir, &candidate)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
                    run.out.on_disk.examine.push((parent.join(&candidate), is_dir));
                    return Ok(true);
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }
}
