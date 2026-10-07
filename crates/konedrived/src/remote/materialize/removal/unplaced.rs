//! [`Materializer::take_off`] for what the folder cannot hold any more
//! ([`Policy::Unplaced`]): the look at what waits there, and the removal of
//! what nothing waits in.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::Path;

use konedrive_fs::placeholder::{read_state, State};
use konedrive_tree::{Kind, Placement, Table, WaitsFor};

#[cfg(test)]
use super::testing;
use super::{device, release_other_names, shown, Policy, Removal, Survey, TakenOff, Unmanaged};
use crate::folder::disk::Probe;
use crate::remote::materialize::{ApplyError, Materializer, Run, Rw};
use crate::status::activity::Kind as EventKind;

/// What a look at something that can no longer be placed found that does
/// not pass by itself.
#[derive(Default)]
struct Stays {
    /// The first of them: what is said.
    first: Option<WaitsFor>,
    /// Every file that is not downloaded and is not where the base has it.
    not_downloaded: Vec<std::path::PathBuf>,
}

/// What the look of [`Materializer::waits`] goes by, the same at every
/// level of the walk: the folder's rules, the filesystem the place is on,
/// this run, and what was found so far that stays.
struct Look<'a> {
    m: &'a Materializer,
    rw: &'a Rw,
    dev: libc::dev_t,
    run: &'a Run,
    stays: Stays,
}

impl Materializer {
    /// [`Self::take_off`] for [`Policy::Unplaced`], once something is found
    /// there. Nothing is touched while anything waits. Otherwise the
    /// objects are forgotten and it goes whole; each file is looked at once
    /// more right before its unlink, and one that holds local work by then
    /// stops the removal where it is: what is left stays an item of the
    /// base, which records it again where it stands, and a later cycle
    /// looks again.
    pub(super) fn take_off_unplaced(&self, dir: &File, name: &OsStr, rel: &Path, survey: &Survey, run: &mut Run) -> Result<TakenOff, ApplyError> {
        let stays = |waits| Ok(TakenOff { removal: Removal::Kept, waits: Some(waits) });
        let Some(rw) = self.mode.read_write() else { return Err(ApplyError::Io(format!("{} is taken off as no longer placed in a read-only folder", rel.display()))) };
        let mut names = Vec::new();
        let waits = self.waits(rw, dir, name, rel, run, &mut names)?;
        // Whether a file that is not downloaded and not where the base has
        // it is one the user renamed is the examination's to say: each is
        // handed over by its name, in every cycle that finds it.
        run.out.on_disk.examine.extend(names.into_iter().map(|at| (at, false)));
        if let Some(waits) = waits {
            return stays(waits);
        }
        #[cfg(test)]
        if let Ok(root) = self.disk.dir(Path::new("")) {
            testing::before_removal(&root);
        }
        let stopped = self.forget(survey, Policy::Unplaced, run)?;
        // Only what was looked at goes: an object that carries another id
        // came since (a placed item the user moved in), and its record was
        // not forgotten.
        let ours: HashSet<&str> = survey.ids.iter().map(String::as_str).collect();
        // A tree, unless it is surely a file: a tree's examination covers a name.
        let is_dir = !matches!(self.disk.probe(dir, name), Ok(Probe::Managed { is_dir: false, .. } | Probe::Unmanaged { is_dir: false }));
        match self.remove_unplaced(rw, &ours, dir, name, rel, run) {
            Ok(None) => {
                run.out.on_disk.taken.extend(survey.ids.iter().cloned());
                Ok(TakenOff { removal: Removal::Gone, waits: None })
            }
            Ok(Some(waits)) => {
                self.settle_stopped(dir, name, &stopped);
                // Whatever stopped it, what is left was forgotten: an
                // examination records it again where it stands.
                run.out.on_disk.examine.push((rel.to_path_buf(), is_dir));
                stays(waits)
            }
            Err(e) => {
                self.settle_stopped(dir, name, &stopped);
                // The objects are forgotten, and what is left of them is
                // still the base's: an examination records them again where
                // they stand, also when the next cycle only finds rows.
                run.out.on_disk.examine.push((rel.to_path_buf(), is_dir));
                Err(e)
            }
        }
    }

    /// What keeps the item whose object stands at `dir/name` (at `rel`) on
    /// disk although the folder cannot hold it any more; `None` when
    /// nothing does. Something waits when an outbox row has a place at or
    /// below it, or when the disk and the base disagree there: what the
    /// user made, changed, moved, renamed or deleted and no examination has
    /// recorded yet, a file open for writing, a state that cannot be read,
    /// a file from elsewhere that is not downloaded, something under an
    /// ignored name that only this computer has, another filesystem.
    fn waits(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, run: &Run, names: &mut Vec<std::path::PathBuf>) -> Result<Option<WaitsFor>, ApplyError> {
        let rows = self.store.call_blocking({ let rel = rel.to_path_buf(); move |s| s.outbox_at_or_under(&rel) })?;
        if !rows.is_empty() {
            return Ok(Some(WaitsFor::Uploads(rows.len() as u64)));
        }
        let Probe::Managed { id, is_dir } = self.disk.probe(dir, name)? else { return Ok(Some(WaitsFor::Changes(shown(rel)))) };
        // What an examination and the outbox settle by themselves is said
        // before what only the user can settle: the place is handed to the
        // watcher for the first.
        let mut look = Look { m: self, rw, dev: device(dir)?, run, stays: Stays::default() };
        let passing = look.differs(dir, name, rel, &id, is_dir)?;
        names.append(&mut look.stays.not_downloaded);
        Ok(passing.or(look.stays.first))
    }

    /// `dir/name` (at `rel`), which nothing was found waiting in, goes with
    /// everything below it. Each object is looked at again right before
    /// its unlink: what [`Self::waits`] did not find there stops the
    /// removal, and is returned — local work, something new, an object
    /// whose id is not one of `ours` (those looked at and forgotten),
    /// another filesystem. That stays, with the directories above it.
    fn remove_unplaced(&self, rw: &Rw, ours: &HashSet<&str>, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<Option<WaitsFor>, ApplyError> {
        self.check_cancel()?;
        let changes = || Ok(Some(WaitsFor::Changes(shown(rel))));
        match self.disk.probe(dir, name)? {
            Probe::Absent => return Ok(None),
            Probe::Managed { id, .. } if !ours.contains(id.as_str()) => return changes(),
            Probe::Managed { is_dir: true, .. } => {
                let sub = self.disk.open_subdir(dir, name)?;
                if device(&sub)? != device(dir)? {
                    return Ok(Some(WaitsFor::MountedInside(shown(rel))));
                }
                let mut stays = None;
                for child in self.disk.list(&sub)? {
                    let stayed = self.remove_unplaced(rw, ours, &sub, &child, &rel.join(&child), run)?;
                    stays = stays.or(stayed);
                }
                if stays.is_some() {
                    return Ok(stays);
                }
                // One that will not go has something in it that came since.
                match self.disk.remove(dir, name, true) {
                    Ok(()) => {}
                    Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) => return changes(),
                    Err(e) => return Err(e.into()),
                }
            }
            Probe::Managed { is_dir: false, .. } => {
                let file = self.disk.open_file(dir, name)?;
                match read_state(&file) {
                    Ok(Some(State::Hydrated)) if self.local_work(&file) => return changes(),
                    Ok(Some(State::Hydrated)) if konedrive_fs::lease::open_for_writing(&file).unwrap_or(true) => return Ok(Some(WaitsFor::OpenForWriting(shown(rel)))),
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return Ok(Some(WaitsFor::UnknownState(shown(rel)))),
                }
                let other_names = self.with_other_names(dir, name)?;
                self.disk.remove(dir, name, false)?;
                if let Some(file) = other_names {
                    #[cfg(test)]
                    if self.disk.dir(Path::new("")).is_ok_and(|root| testing::stops_after_unlink(&root)) {
                        return Err(ApplyError::Io(format!("{}: stopped after the unlink (test)", rel.display())));
                    }
                    release_other_names(&file, rel);
                }
            }
            Probe::Unmanaged { is_dir } => {
                if !matches!(self.unmanaged(rw, dir, name)?, Unmanaged::Ours | Unmanaged::Beside) {
                    return changes();
                }
                // The daemon's own, or nothing to lose. One that will not
                // go (a directory with something in it) stays for now.
                return match self.disk.remove(dir, name, is_dir) {
                    Ok(()) => Ok(None),
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
                    Err(_) => changes(),
                };
            }
        }
        run.out.counts.deleted += 1;
        run.note(EventKind::Removed, rel, None);
        Ok(None)
    }
}

impl Look<'_> {
    /// `waits` is what stays, unless something found before it is.
    fn stay(&mut self, waits: WaitsFor) {
        self.stays.first.get_or_insert(waits);
    }

    /// [`Materializer::waits`] for the object of item `id` at `dir/name`, which is
    /// where the base has it, and for everything below it: the directory
    /// and the base are walked side by side. Returned: the first thing
    /// found that an examination records or that passes by itself. The
    /// first thing found that stays until the user does something about it
    /// is kept ([`Self::stay`]), and the walk goes on.
    fn differs(&mut self, dir: &File, name: &OsStr, rel: &Path, id: &str, is_dir: bool) -> Result<Option<WaitsFor>, ApplyError> {
        self.m.check_cancel()?;
        if !is_dir {
            let file = self.m.disk.open_file(dir, name)?;
            return Ok(match read_state(&file) {
                Ok(Some(State::Hydrated)) if self.m.local_work(&file) => Some(WaitsFor::Changes(shown(rel))),
                Ok(Some(State::Hydrated)) if konedrive_fs::lease::open_for_writing(&file).unwrap_or(true) => Some(WaitsFor::OpenForWriting(shown(rel))),
                Ok(Some(_)) => None,
                Ok(None) | Err(_) => {
                    self.stay(WaitsFor::UnknownState(shown(rel)));
                    None
                }
            });
        }
        let sub = self.m.disk.open_subdir(dir, name)?;
        if device(&sub)? != self.dev {
            self.stay(WaitsFor::MountedInside(shown(rel)));
            return Ok(None);
        }
        let folder = id.to_owned();
        let mut base: HashMap<OsString, konedrive_tree::Row> = self
            .m
            .store
            .call_blocking(move |s| s.children(Table::Items, &folder))?
            .into_iter()
            .filter(|row| row.placement == Placement::Placed)
            .map(|row| (OsString::from(&row.name), row))
            .collect();
        for child in self.m.disk.list(&sub)? {
            let at = rel.join(&child);
            match self.m.disk.probe(&sub, &child)? {
                Probe::Absent => {}
                Probe::Managed { id, is_dir } => {
                    let expected = base.get(&child).is_some_and(|row| row.id == id && (row.kind == Kind::Folder) == is_dir);
                    if !expected {
                        // Moved, renamed or copied here, or from elsewhere.
                        // A file that is not downloaded may be one the user
                        // renamed, whose row an examination records; one
                        // that is not the item's object has nothing to send
                        // and stays listed until the user removes it. Which
                        // of the two is the examination's to say, so it is
                        // said as what stays, and the place is handed over.
                        let empty = !is_dir && !matches!(self.m.disk.open_file(&sub, &child).map(|file| read_state(&file)), Ok(Ok(Some(State::Hydrated))));
                        if empty {
                            self.stay(WaitsFor::NotDownloaded(shown(&at)));
                            self.stays.not_downloaded.push(at);
                            continue;
                        }
                        return Ok(Some(WaitsFor::Changes(shown(&at))));
                    }
                    base.remove(&child);
                    // A child OneDrive moved out, left here because its new
                    // name is held by a local change: the folder stays for
                    // it.
                    if self.run.out.pending.unsettled.contains(&id) || self.run.left.contains(&id) {
                        let moved = self.m.store.call_blocking({ let id = id.clone(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|to| to.placed);
                        if moved {
                            self.stay(WaitsFor::MovedAway(shown(&at)));
                            continue;
                        }
                    }
                    if let Some(waits) = self.differs(&sub, &child, &at, &id, is_dir)? {
                        return Ok(Some(waits));
                    }
                }
                Probe::Unmanaged { is_dir } => {
                    if is_dir && self.m.disk.open_subdir(&sub, &child).and_then(|below| device(&below)).is_ok_and(|below| below != self.dev) {
                        self.stay(WaitsFor::MountedInside(shown(&at)));
                        continue;
                    }
                    match self.m.unmanaged(self.rw, &sub, &child)? {
                        Unmanaged::Ours | Unmanaged::Beside => {}
                        // Never uploaded, and only here: it is not removed.
                        Unmanaged::Theirs if self.rw.ignore.matches(&child) => self.stay(WaitsFor::LocalOnly(shown(&at))),
                        Unmanaged::Theirs | Unmanaged::Folder => return Ok(Some(WaitsFor::Changes(shown(&at)))),
                    }
                }
            }
        }
        // What the base has here and the disk does not: deleted or moved by
        // the user, which an examination proves by the recorded object. One
        // this run took away itself, or with no object on record, is no
        // change anybody made here.
        for (name, row) in base {
            if self.run.out.on_disk.taken.contains(&row.id) {
                continue;
            }
            if self.run.moved_from.contains_key(&row.id) {
                // Moved to the holding directory for its new place. One
                // that could not be placed is put back here: its folder
                // stays for it.
                let held = match self.m.holding_if_any()? {
                    Some(holding) => !matches!(self.m.disk.probe(&holding, OsStr::new(&row.id))?, Probe::Absent),
                    None => false,
                };
                if held {
                    self.stay(WaitsFor::MovedAway(shown(&rel.join(&name))));
                }
                continue;
            }
            if self.m.store.call_blocking(move |s| s.local_handle(&row.id))?.is_some() {
                return Ok(Some(WaitsFor::Changes(shown(&rel.join(name)))));
            }
        }
        Ok(None)
    }
}
