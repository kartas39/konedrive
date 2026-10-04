//! What can no longer be placed in a read-write folder (issue #104): an
//! item OneDrive still has and the folder cannot hold goes from the disk
//! whole or waits whole, once everything else is placed; it yields its name
//! to another item by stepping aside.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_tree::Table;

use super::Rw;
use crate::folder::disk::Probe;
use crate::local::names::copy_name;
use crate::remote::materialize::removal::Policy;
use crate::remote::materialize::{ApplyError, Materializer, Run};

/// The items phase 1 found that can no longer be placed, for
/// [`Materializer::after_placement`].
#[derive(Default)]
pub(in crate::remote::materialize) struct Unplaced {
    tops: Vec<Top>,
    /// What is below a topmost item and follows it: the item, and where it
    /// is (as the tops' `covers`).
    follow: Vec<(String, PathBuf)>,
}

/// The topmost item of a subtree that can no longer be placed.
struct Top {
    id: String,
    /// Its place as phase 1 saw it: what is below that follows it.
    covers: PathBuf,
    /// Where its object stands and whether that is a directory; `None` when
    /// this pass does not look at it.
    stands: Option<(PathBuf, bool)>,
}

impl Unplaced {
    pub(in crate::remote::materialize) fn top(&mut self, id: &str, covers: &Path, stands: Option<(PathBuf, bool)>) {
        self.tops.push(Top { id: id.to_owned(), covers: covers.to_path_buf(), stands });
    }

    pub(in crate::remote::materialize) fn follows(&mut self, id: &str, at: &Path) {
        self.follow.push((id.to_owned(), at.to_path_buf()));
    }
}

impl Materializer {
    /// Phase 3 of both scopes: what can no longer be placed, once what
    /// OneDrive moved out of it is placed — one still in the holding
    /// directory by then keeps its folder. Each topmost item goes whole or
    /// waits whole, deepest first ([`Self::take_off_or_wait`]). What
    /// follows one goes its way: taken by the base with it, or left as it
    /// is, its change waiting.
    pub(in crate::remote::materialize) fn after_placement(&self, unplaced: Unplaced, run: &mut Run) -> Result<(), ApplyError> {
        let Unplaced { mut tops, follow } = unplaced;
        tops.sort_by_key(|top| std::cmp::Reverse(top.covers.components().count()));
        for top in &tops {
            self.check_cancel()?;
            let Some((stands, is_dir)) = &top.stands else { continue };
            if self.take_off_or_wait(&top.id, stands, *is_dir, run)? {
                run.left.insert(top.id.clone());
                run.out.pending.unsettled.insert(top.id.clone());
            }
        }
        for (id, at) in follow {
            let top = tops.iter().find(|top| at.starts_with(&top.covers));
            if top.is_some_and(|top| run.out.on_disk.taken.contains(&top.id)) {
                run.out.on_disk.taken.insert(id);
            } else {
                run.left.insert(id.clone());
                run.out.pending.unsettled.insert(id);
            }
        }
        Ok(())
    }

    /// The object of item `id`, which can no longer be placed, is taken off
    /// the disk at `rel`, where this run found it, or waits there
    /// ([`Self::wait_to_leave`]); whether it waits. Where it stepped aside
    /// in this run, there. Only its own object: should something else stand
    /// at the path, nothing is touched and the item waits for the next
    /// cycle to look.
    fn take_off_or_wait(&self, id: &str, rel: &Path, is_dir: bool, run: &mut Run) -> Result<bool, ApplyError> {
        // Where it stands now: where it stepped aside to in this run, or in
        // its folder, wherever this run put that.
        let stands = self.stands_now(id, rel, run)?;
        let rel = stands.as_path();
        let (parent, name) = (rel.parent().unwrap_or(Path::new("")), rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?);
        let dir = self.disk.dir(parent)?;
        let waits = match self.disk.probe(&dir, name)? {
            Probe::Managed { id: there, .. } if there == id => self.take_off(&dir, name, rel, Policy::Unplaced, run)?.waits,
            _ => Some(konedrive_tree::WaitsFor::Cycle),
        };
        match waits {
            Some(waits) => {
                self.wait_to_leave(id, rel, is_dir, waits, run)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Where the object of item `id`, which this run found at `rel`, stands
    /// by now: at the name it stepped aside to, or under the name the base
    /// has, in the directory that carries its folder's id — which this run
    /// may have moved since it looked.
    fn stands_now(&self, id: &str, rel: &Path, run: &Run) -> Result<PathBuf, ApplyError> {
        if let Some(aside) = run.aside.get(id) {
            return Ok(aside.clone());
        }
        let Some(base) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Items, &id) })? else { return Ok(rel.to_path_buf()) };
        let Some(folder) = base.parent_id.as_deref() else { return Ok(rel.to_path_buf()) };
        Ok(self.dir_of(folder, run)?.map_or_else(|| rel.to_path_buf(), |dir| dir.join(&base.name)))
    }

    /// Where the directory of folder `id` stands now: of the places it can
    /// be — where it stepped aside to, where the new tree places it, where
    /// the base has it — the first at which a directory carries its id.
    pub(super) fn dir_of(&self, id: &str, run: &Run) -> Result<Option<PathBuf>, ApplyError> {
        if id == self.root_item_id {
            return Ok(Some(PathBuf::new()));
        }
        let asked = id.to_owned();
        let (new, base) = self.store.call_blocking(move |s| Ok((s.locate(Table::Staging, &asked)?, s.locate(Table::Items, &asked)?)))?;
        let places = run.aside.get(id).cloned().into_iter().chain([new, base].into_iter().flatten().filter(|at| at.placed).map(|at| at.rel));
        for place in places {
            let (Some(name), parent) = (place.file_name(), place.parent().unwrap_or(Path::new(""))) else { continue };
            let Ok(dir) = self.disk.dir(parent) else { continue };
            if matches!(self.disk.probe(&dir, name), Ok(Probe::Managed { id: there, is_dir: true }) if there == id) {
                return Ok(Some(place));
            }
        }
        Ok(None)
    }

    /// The yielding rule. The object of item `id` at `dir/name` (at `rel`)
    /// can no longer be placed, and another item takes its name in this
    /// run: it is renamed in its directory to the first free copy name
    /// (`name-<machine>.ext`), never over anything, keeping its attributes,
    /// and the base follows to that name at once
    /// ([`TreeStore::step_aside`]). The base and the disk agree, so no
    /// examination takes the new name for a rename made here and nothing is
    /// sent; the item leaves from there, or waits there, as it would have
    /// where it stood. Always this side yields, so no item waits for a name
    /// that waits for it.
    ///
    /// [`TreeStore::step_aside`]: konedrive_tree::TreeStore::step_aside
    ///
    /// Whether it stepped aside: not an item that is itself a mount point,
    /// which cannot be renamed. That one keeps its name and waits, saying
    /// so, and what takes its name waits for it.
    pub(in crate::remote::materialize) fn step_aside(&self, dir: &File, name: &OsStr, rel: &Path, id: &str, run: &mut Run) -> Result<bool, ApplyError> {
        // Only a read-write folder has anything that leaves.
        let Some(rw) = self.mode.read_write() else { return Ok(false) };
        let original = name.to_str().ok_or_else(|| ApplyError::Io(format!("{} has a name that is not UTF-8", rel.display())))?;
        for n in 1..=100 {
            let candidate = copy_name(original, &rw.machine, n);
            match self.disk.rename(dir, name, dir, OsStr::new(&candidate)) {
                Ok(()) => {
                    let aside = rel.with_file_name(&candidate);
                    let (id, from, to) = (id.to_owned(), rel.to_path_buf(), aside.clone());
                    run.aside.insert(id.clone(), aside.clone());
                    self.store.call_blocking(move |s| s.step_aside(&id, &from, &to, &candidate, false))?;
                    tracing::info!("{} can no longer be placed here and its name is taken by another item: it is {} for now", rel.display(), aside.display());
                    return Ok(true);
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
        Err(ApplyError::Io(format!("no free name to step {} aside to", rel.display())))
    }

    /// The object of item `id`, which can no longer be placed, is not where
    /// the base has it (`rel`): whether it stands beside that place under a
    /// copy name of the base's — a step aside whose record a stop of the
    /// daemon cut off. Then the base takes the name now, and a rename an
    /// examination recorded for it since goes; where it stands.
    pub(super) fn stepped_aside_before(&self, rw: &Rw, id: &str, rel: &Path) -> Result<Option<PathBuf>, ApplyError> {
        let (Some(name), parent) = (rel.file_name().and_then(OsStr::to_str), rel.parent().unwrap_or(Path::new(""))) else { return Ok(None) };
        let Ok(dir) = self.disk.dir(parent) else { return Ok(None) };
        for n in 1..=100 {
            let candidate = copy_name(name, &rw.machine, n);
            match self.disk.probe(&dir, OsStr::new(&candidate))? {
                Probe::Managed { id: there, .. } if there == id => {
                    let aside = rel.with_file_name(&candidate);
                    let (id, from, to) = (id.to_owned(), rel.to_path_buf(), aside.clone());
                    self.store.call_blocking(move |s| s.step_aside(&id, &from, &to, &candidate, true))?;
                    tracing::info!("{} was stepped aside before a stop of the daemon: it is {} in the base too now", rel.display(), aside.display());
                    return Ok(Some(aside));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Item `id`, at `rel`, can no longer be placed and cannot go yet
    /// ([`Policy::Unplaced`]): its change waits, with what it waits for, and
    /// so does everything the base or the new tree has below it — but for
    /// what this run took off the disk or took out of it to place
    /// elsewhere. The base keeps it placed, with its recorded object. Where
    /// the disk holds what no examination has recorded, the place is handed
    /// to the watcher.
    pub(super) fn wait_to_leave(&self, id: &str, rel: &Path, is_dir: bool, waits: konedrive_tree::WaitsFor, run: &mut Run) -> Result<(), ApplyError> {
        use konedrive_tree::WaitsFor;
        tracing::debug!("{} can no longer be placed here; it stays until nothing in it waits ({waits})", rel.display());
        let folder = id.to_owned();
        let below = self.store.call_blocking(move |s| {
            let mut ids = s.descendants(Table::Staging, &folder)?;
            ids.extend(s.descendants(Table::Items, &folder)?);
            Ok(ids)
        })?;
        for id in below {
            if !run.out.on_disk.taken.contains(&id) && !run.moved_from.contains_key(&id) {
                run.out.pending.unsettled.insert(id);
            }
        }
        run.out.pending.unsettled.insert(id.to_owned());
        let said = waits.to_string();
        if matches!(waits, WaitsFor::Changes(_) | WaitsFor::OpenForWriting(_)) {
            run.out.on_disk.examine.push((rel.to_path_buf(), is_dir));
        }
        run.out.pending.waits.push((id.to_owned(), said));
        Ok(())
    }
}
