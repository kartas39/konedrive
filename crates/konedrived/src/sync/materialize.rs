//! Makes the folder match the tree.
//!
//! Phase 1 moves every item of ours that is not where the new tree wants it
//! into the holding directory under its item id, deepest first, so that a move
//! never changes the path of something still to be moved. Phase 2 walks the
//! new tree top down: each item is found in place, taken back from the holding
//! directory, or created. Phase 3 deletes what is left in the holding
//! directory, rescuing any file that holds local work. A crash
//! anywhere leaves only things the next Full reconcile recognises by item id
//! — but for a new folder's temporary directory killed before its label,
//! which the next attempt to make that folder clears (`labelled_dir`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{self, read_state, stamp_matches, PlaceholderSpec, State, LOCKED_FILE_MODE, OPEN_FILE_MODE};
use tokio_util::sync::CancellationToken;

use super::disk::{Disk, Probe, Scanned, HOLDING, NEW_PREFIX};
use super::activity::Kind as EventKind;
use super::helper::HelperLink;
use super::source::ContentSource;
use super::InodeLocks;
use crate::tree::{Kind, Placement, Row, Store, Table, TreeError};

pub enum Scope {
    /// Scan the folder and match it to the whole tree.
    Full,
    /// Only these items (a delta's), and what comes into view with them.
    Changed(Vec<String>),
}

/// A file downloaded here whose content changed in the cloud: fetches
/// the new version beside it and swaps it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub id: String,
    pub rel: PathBuf,
    pub ctag: String,
    /// The new version's size, to check the disk can hold it beside the old.
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Applied {
    pub created: u64,
    pub moved: u64,
    pub deleted: u64,
    pub updated: u64,
    /// Files being filled or freed right now; the next cycle looks again.
    pub deferred: u64,
    /// Local versions moved out of the way, each a conflict
    ///.
    pub rescued: Vec<Rescued>,
    pub replacements: Vec<Replacement>,
    /// What a Changed scope did, item by item, for the activity log (spec
    /// §16.1). A Full scope leaves it empty: it is one `listed` event, not
    /// one per item.
    pub changes: Vec<Changed>,
    /// Files made, and files or folders moved, inside a folder a pin keeps
    /// on this device, relative to the root: what the sync queues for
    /// download once the reconcile is done.
    pub pinned: Vec<PathBuf>,
}

/// A local version a reconcile moved out of the way (§16.3: "the
/// daemon records what the materializer rescued").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rescued {
    /// Where it was, relative to the root.
    pub original: PathBuf,
    /// Where it is now, as a full path.
    pub rescued: PathBuf,
}

/// One thing an incremental reconcile did to the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// `Added`, `Updated`, `Removed` or `Moved`.
    pub kind: EventKind,
    /// Relative to the root: where the item is now, or was, if removed.
    pub rel: PathBuf,
    /// Where a moved item was, relative to the root.
    pub from: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("the folder does not match the stored tree ({0})")]
    NeedFull(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the helper did not mark {0}: {1}")]
    Mark(PathBuf, String),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Tree(#[from] TreeError),
}

impl From<std::io::Error> for ApplyError {
    fn from(e: std::io::Error) -> Self {
        ApplyError::Io(e.to_string())
    }
}

pub struct Materializer {
    pub disk: Disk,
    pub store: Store,
    /// For a folder with interception: every directory made is marked through
    /// it before anything is put in it (invariant M1).
    pub link: Option<HelperLink>,
    /// To wait for the helper from this blocking thread.
    pub runtime: tokio::runtime::Handle,
    pub locks: InodeLocks,
    pub root_item_id: String,
    /// `rescued/<timestamp>` for this cycle.
    pub rescue_into: PathBuf,
    pub cancel: CancellationToken,
}

#[derive(Default)]
struct Run {
    out: Applied,
    /// Where each item moved to the holding directory came from — the path a
    /// rescued file is shown under.
    moved_from: HashMap<String, PathBuf>,
    /// Directories made this run, locked once everything is in them.
    made: Vec<PathBuf>,
    /// The Changed scope: an item of ours in the way that is not in it means
    /// the folder does not match the stored tree.
    scope: Option<HashSet<String>>,
    /// The holding directory has been marked by this run (invariant M1).
    holding_marked: bool,
    /// Whether a pin keeps the directory at each path on this device, as far
    /// as this run has asked: a directory's answer is read once.
    pinned_dirs: HashMap<PathBuf, bool>,
}

impl Run {
    /// Notes what a Changed scope did to `rel`; a Full scope notes nothing
    /// (see [`Applied::changes`]).
    fn note(&mut self, kind: EventKind, rel: &Path, from: Option<PathBuf>) {
        if self.scope.is_some() {
            self.out.changes.push(Changed { kind, rel: rel.to_path_buf(), from });
        }
    }
}

impl Materializer {
    pub fn apply(&self, scope: Scope) -> Result<Applied, ApplyError> {
        self.apply_keeping(scope, &mut Vec::new())
    }

    /// [`apply`](Self::apply), which also hands over, when it fails, what it
    /// had rescued by then (into `rescued`): those files are out of the way
    /// whatever comes next, and a Full reconcile run after a Changed one
    /// finds nothing left to rescue there.
    pub fn apply_keeping(&self, scope: Scope, rescued: &mut Vec<Rescued>) -> Result<Applied, ApplyError> {
        let mut run = Run::default();
        let result = self.apply_run(scope, &mut run);
        if result.is_err() {
            rescued.append(&mut run.out.rescued);
        }
        result.map(|()| run.out)
    }

    fn apply_run(&self, scope: Scope, run: &mut Run) -> Result<(), ApplyError> {
        match scope {
            Scope::Full => self.full(run)?,
            Scope::Changed(ids) => self.changed(ids, run)?,
        }
        self.drain_holding(run)?;
        for rel in run.made.iter().rev() {
            if let Ok(dir) = self.disk.dir(rel) {
                self.disk.lock_dir(&dir)?;
            }
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), ApplyError> {
        if self.cancel.is_cancelled() {
            return Err(ApplyError::Cancelled);
        }
        Ok(())
    }

    fn full(&self, run: &mut Run) -> Result<(), ApplyError> {
        self.check_cancel()?;
        let scanned = self.disk.scan(&self.root_item_id)?;
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for entry in &scanned {
            if let Some(id) = &entry.id {
                *id_counts.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        let mut misplaced: Vec<&Scanned> = Vec::new();
        for entry in &scanned {
            let Some(id) = &entry.id else { continue };
            if is_leftover_replacement(entry, id, &id_counts) {
                self.check_cancel()?;
                self.discard_leftover_replacement(entry, run)?;
                continue;
            }
            if self.is_misplaced(entry, id)? {
                misplaced.push(entry);
            }
        }
        misplaced.sort_by(|a, b| b.depth.cmp(&a.depth));
        for entry in misplaced {
            self.check_cancel()?;
            self.to_holding(&entry.rel, entry.id.as_deref().expect("filtered above"), run)?;
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.lock_dir(&root)?;
        let mut queue = VecDeque::from([(self.root_item_id.clone(), PathBuf::new())]);
        while let Some((id, rel)) = queue.pop_front() {
            self.check_cancel()?;
            let children = self.store.with(|s| s.children(Table::Staging, &id))?;
            for row in children {
                if row.placement != Placement::Placed {
                    continue;
                }
                let placed = self.place(&row, &rel, run, true)?;
                if row.kind == Kind::Folder {
                    queue.push_back((row.id.clone(), placed));
                }
            }
        }
        Ok(())
    }

    fn is_misplaced(&self, entry: &Scanned, id: &str) -> Result<bool, ApplyError> {
        let Some(row) = self.store.with(|s| s.get(Table::Staging, id))? else {
            return Ok(true);
        };
        Ok(row.placement != Placement::Placed
            || row.parent_id != entry.parent_id
            || entry.rel.file_name() != Some(OsStr::new(&row.name))
            || (row.kind == Kind::Folder) != entry.is_dir)
    }

    fn changed(&self, ids: Vec<String>, run: &mut Run) -> Result<(), ApplyError> {
        // What an earlier run left in the holding directory is not this
        // delta's to drain; a Full reconcile sorts it out by item id.
        if self.holding_if_any()?.is_some() {
            return Err(ApplyError::NeedFull(format!("{HOLDING} is left from an earlier run")));
        }
        let mut scope: HashSet<String> = ids.iter().cloned().collect();
        // The root's own entry changes with every change below it; it is the
        // folder itself, never something to move.
        scope.remove(&self.root_item_id);
        for id in &ids {
            let new = self.store.with(|s| s.locate(Table::Staging, id))?;
            let old = self.store.with(|s| s.locate(Table::Items, id))?;
            let comes_into_view = new.as_ref().is_some_and(|l| l.placed) && !old.as_ref().is_some_and(|l| l.placed);
            if comes_into_view {
                scope.extend(self.store.with(|s| s.descendants(Table::Staging, id))?);
            }
        }
        run.scope = Some(scope.clone());

        // Phase 1, by where things are now, deepest first.
        let mut here = Vec::new();
        for id in &scope {
            if let Some(old) = self.store.with(|s| s.locate(Table::Items, id))?.filter(|l| l.placed) {
                here.push((id.clone(), old));
            }
        }
        here.sort_by(|a, b| b.1.depth.cmp(&a.1.depth));
        for (id, old) in &here {
            self.check_cancel()?;
            let parent = old.rel.parent().unwrap_or(Path::new(""));
            let name = old.rel.file_name().ok_or_else(|| ApplyError::NeedFull(format!("{id} has no name")))?;
            let dir = self.disk.dir(parent).map_err(|e| ApplyError::NeedFull(format!("{}: {e}", parent.display())))?;
            match self.disk.probe(&dir, name)? {
                Probe::Managed { id: found, .. } if &found == id => {}
                other => return Err(ApplyError::NeedFull(format!("{} should be {id} and is {other:?}", old.rel.display()))),
            }
            let old_row = self.store.with(|s| s.get(Table::Items, id))?;
            let new_row = self.store.with(|s| s.get(Table::Staging, id))?;
            let stays = matches!((&old_row, &new_row), (Some(o), Some(n))
                if n.placement == Placement::Placed && n.parent_id == o.parent_id && n.name == o.name);
            if !stays {
                self.to_holding(&old.rel, id, run)?;
            }
        }

        // Phase 2, by where things belong, shallowest first.
        let mut there = Vec::new();
        for id in &scope {
            let row = self.store.with(|s| s.get(Table::Staging, id))?;
            let new = self.store.with(|s| s.locate(Table::Staging, id))?;
            if let (Some(row), Some(new)) = (row, new) {
                if new.placed {
                    there.push((row, new));
                }
            }
        }
        there.sort_by(|a, b| a.1.depth.cmp(&b.1.depth));
        // Folders placed — and so checked — by this phase.
        let mut placed: HashSet<String> = HashSet::new();
        for (row, new) in &there {
            self.check_cancel()?;
            let parent = new.rel.parent().unwrap_or(Path::new(""));
            self.check_parent(row, parent, &placed)?;
            self.place(row, parent, run, false)?;
            if row.kind == Kind::Folder {
                placed.insert(row.id.clone());
            }
        }
        Ok(())
    }

    /// The Changed scope opens an item's folder by its path. Unless that is
    /// the root, or a folder this run has just placed, the directory there is
    /// checked first to be the folder the tree says — anything else there
    /// (a stranger of the same name, put there past the lock) is the folder
    /// not matching the stored tree.
    fn check_parent(&self, row: &Row, parent: &Path, placed: &HashSet<String>) -> Result<(), ApplyError> {
        let Some(parent_id) = row.parent_id.as_deref() else {
            return Ok(());
        };
        if parent.as_os_str().is_empty() || placed.contains(parent_id) {
            return Ok(());
        }
        let above = parent.parent().unwrap_or(Path::new(""));
        let name = parent.file_name().ok_or_else(|| ApplyError::NeedFull(format!("{} has no name", parent.display())))?;
        let dir = self.disk.dir(above).map_err(|e| ApplyError::NeedFull(format!("{}: {e}", above.display())))?;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id, is_dir: true } if id == parent_id => Ok(()),
            other => Err(ApplyError::NeedFull(format!("{} should be the folder {parent_id} and is {other:?}", parent.display()))),
        }
    }

    /// Makes `row` exist as `parent_rel/<name>` and returns that path.
    fn place(&self, row: &Row, parent_rel: &Path, run: &mut Run, full: bool) -> Result<PathBuf, ApplyError> {
        let rel = parent_rel.join(&row.name);
        let dir = self.disk.dir(parent_rel)?;
        let name = OsStr::new(&row.name);
        let is_folder = row.kind == Kind::Folder;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id, is_dir } if id == row.id && is_dir == is_folder => {
                if !is_folder {
                    self.check_file(&dir, name, row, &rel, run)?;
                }
                if full {
                    // Not a file a fill holds (A-M4): it is left for the next
                    // Full reconcile.
                    self.disk.enforce_mode(&dir, name, |file| Ok(self.locks.try_lock(super::InodeKey::of(file)?)))?;
                }
                return Ok(rel);
            }
            Probe::Managed { id, .. } if id == row.id => {
                // Its own id with the wrong kind: nothing a Graph id does.
                // Not trusted, not thrown away.
                self.rescue(&dir, name, &rel, run)?;
            }
            Probe::Managed { id, .. } => {
                if run.scope.as_ref().is_some_and(|scope| !scope.contains(&id)) {
                    return Err(ApplyError::NeedFull(format!("{id} is in the way at {}", rel.display())));
                }
                self.to_holding(&rel, &id, run)?;
            }
            Probe::Unmanaged { .. } => self.rescue(&dir, name, &rel, run)?,
            Probe::Absent => {}
        }
        if let Some(holding) = self.holding_if_any()? {
            if let Probe::Managed { id, is_dir } = self.disk.probe(&holding, OsStr::new(&row.id))? {
                if id == row.id && is_dir == is_folder {
                    if is_folder {
                        // A folder made by a cycle whose marking failed waits
                        // here unmarked; it is marked before its real name
                        // shows it (invariant M1). Marking twice is harmless.
                        let waiting = self.disk.open_subdir(&holding, OsStr::new(&row.id))?;
                        self.mark(&waiting, &rel)?;
                    }
                    self.disk.rename(&holding, OsStr::new(&row.id), &dir, name)?;
                    run.out.moved += 1;
                    let from = run.moved_from.get(&row.id).cloned();
                    run.note(EventKind::Moved, &rel, from);
                    self.note_if_pinned(&rel, run);
                    if !is_folder {
                        self.check_file(&dir, name, row, &rel, run)?;
                    }
                    return Ok(rel);
                }
            }
        }
        self.create(&dir, row, &rel, run)?;
        run.note(EventKind::Added, &rel, None);
        Ok(rel)
    }

    fn create(&self, dir: &File, row: &Row, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        match row.kind {
            Kind::Folder => {
                let temp = format!("{NEW_PREFIX}{}", row.id);
                let made = self.labelled_dir(dir, &temp, row, rel, run)?;
                self.mark(&made, rel)?;
                self.disk.rename(dir, OsStr::new(&temp), dir, OsStr::new(&row.name))?;
                run.made.push(rel.to_path_buf());
            }
            Kind::File => {
                let spec = PlaceholderSpec {
                    item_id: &row.id,
                    size: row.size,
                    mtime: cloud_time(row),
                    ctag: row.ctag.as_deref(),
                    mode: if self.disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE },
                };
                self.disk.writable(dir, || placeholder::create_placeholder_with(dir, &row.name, &spec))?;
                self.note_if_pinned(rel, run);
            }
        }
        run.out.created += 1;
        Ok(())
    }

    /// Notes `rel` in [`Applied::pinned`] when the folder it is in is
    /// pinned — by its own pin or one above it. A pin that cannot be read is
    /// logged and not followed: the next sweep finds the file.
    fn note_if_pinned(&self, rel: &Path, run: &mut Run) {
        let parent = rel.parent().unwrap_or(Path::new(""));
        match self.dir_pinned(parent, run) {
            Ok(true) => run.out.pinned.push(rel.to_path_buf()),
            Ok(false) => {}
            Err(e) => tracing::warn!("cannot tell whether {} is kept on this device: {e}", parent.display()),
        }
    }

    /// Whether a pin keeps the directory at `rel` on this device: its own,
    /// or one on a directory above it up to the root.
    fn dir_pinned(&self, rel: &Path, run: &mut Run) -> std::io::Result<bool> {
        if let Some(&pinned) = run.pinned_dirs.get(rel) {
            return Ok(pinned);
        }
        let pinned = placeholder::read_pin(&self.disk.dir(rel)?)?
            || match rel.parent() {
                Some(above) => self.dir_pinned(above, run)?,
                None => false,
            };
        run.pinned_dirs.insert(rel.to_path_buf(), pinned);
        Ok(pinned)
    }

    /// A new folder under its temporary name `temp`, labelled with its id.
    ///
    /// The name can be taken already: a kill
    /// between `mkdirat` and the label, or a label the disk had no room for,
    /// leaves a directory there with no id. A scan passes over what has no
    /// id, so nothing cleared it, and every later reconcile failed `EEXIST`
    /// here — for good. So a taken name is looked at: this folder's own,
    /// already labelled, is used as it is; an empty directory with no id is
    /// removed; anything else is rescued — nobody but this daemon
    /// makes that name, so whatever is in it came from outside.
    fn labelled_dir(&self, dir: &File, temp: &str, row: &Row, rel: &Path, run: &mut Run) -> Result<File, ApplyError> {
        let make = || self.disk.writable(dir, || placeholder::create_dir_item(dir, temp, &row.id));
        match make() {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            other => return Ok(other?),
        }
        let name = OsStr::new(temp);
        match self.disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: true } if id == row.id => return Ok(self.disk.open_subdir(dir, name)?),
            Probe::Unmanaged { is_dir: true } if self.disk.list(&self.disk.open_subdir(dir, name)?)?.is_empty() => {
                self.disk.remove(dir, name, true)?;
            }
            Probe::Absent => {}
            _ => self.rescue(dir, name, &rel.with_file_name(temp), run)?,
        }
        Ok(make()?)
    }

    fn mark(&self, dir: &File, rel: &Path) -> Result<(), ApplyError> {
        if let Some(link) = &self.link {
            self.runtime
                .block_on(link.mark_dir(dir))
                .map_err(|e| ApplyError::Mark(rel.to_path_buf(), e.to_string()))?;
        }
        Ok(())
    }

    fn holding_if_any(&self) -> Result<Option<File>, ApplyError> {
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

    fn to_holding(&self, rel: &Path, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        if parent == Path::new(HOLDING) && name == OsStr::new(id) {
            return Ok(());
        }
        let holding = self.holding(run)?;
        let dir = self.disk.dir(parent)?;
        self.disk.rename(&dir, name, &holding, OsStr::new(id))?;
        run.moved_from.entry(id.to_owned()).or_insert_with(|| rel.to_path_buf());
        Ok(())
    }

    fn drain_holding(&self, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let shown = name
                .to_str()
                .and_then(|id| run.moved_from.get(id).cloned())
                .unwrap_or_else(|| PathBuf::from(HOLDING).join(&name));
            let deleted = run.out.deleted;
            self.delete_tree(&holding, &name, &shown, run)?;
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

    fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.rescue(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} held local work the cloud's change would have lost; it is kept at {}",
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
    fn discard_leftover_replacement(&self, entry: &Scanned, run: &mut Run) -> Result<(), ApplyError> {
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

    /// A file already in place, and what its content needs:
    /// a placeholder takes the new size, time and cTag in place; a downloaded
    /// file of another version is queued for replacement (§7.3), or rescued
    /// first when it holds local work (§9.3); a file being filled or freed up
    /// right now is left for the next cycle.
    ///
    /// A placeholder's content is told by its cTag and size alone, never by
    /// its time: every write of a fill moves the
    /// time to now, and a fill that stopped part-way — a restart, a failure —
    /// leaves the time wrong over a checkpoint worth keeping. Taken for a
    /// new version, the whole partial download was punched away at the first
    /// Full reconcile after a restart. Only the time is put back then.
    fn check_file(&self, dir: &File, name: &OsStr, row: &Row, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        use std::os::unix::fs::MetadataExt;
        let file = self.disk.open_file(dir, name)?;
        let local_ctag = placeholder::read_ctag(&file).ok().flatten();
        match read_state(&file) {
            Ok(Some(State::OnlineOnly)) => {
                let meta = file.metadata()?;
                let same_content = local_ctag.as_deref() == row.ctag.as_deref() && meta.len() == row.size;
                if !same_content {
                    if self.update_placeholder(file, row, run)? {
                        run.note(EventKind::Updated, rel, None);
                    }
                } else if meta.mtime() != row.mtime {
                    self.put_time_back(file, row, run)?;
                }
                Ok(())
            }
            Ok(Some(State::Hydrated)) => {
                if local_ctag.is_some() && local_ctag.as_deref() == row.ctag.as_deref() {
                    return Ok(());
                }
                if holds_local_work(&file) {
                    drop(file);
                    self.rescue(dir, name, rel, run)?;
                    self.create(dir, row, rel, run)?;
                    run.note(EventKind::Updated, rel, None);
                    return Ok(());
                }
                if let Some(ctag) = &row.ctag {
                    run.out.replacements.push(Replacement { id: row.id.clone(), rel: rel.to_path_buf(), ctag: ctag.clone(), size: row.size });
                }
                Ok(())
            }
            Ok(Some(State::Hydrating | State::Dehydrating)) => {
                run.out.deferred += 1;
                Ok(())
            }
            // Ours by its id, in no state anyone can vouch for.
            Ok(None) | Err(_) => {
                let work = holds_local_work(&file);
                drop(file);
                if work {
                    self.rescue(dir, name, rel, run)?;
                } else {
                    self.disk.remove(dir, name, false)?;
                }
                self.create(dir, row, rel, run)?;
                run.note(EventKind::Updated, rel, None);
                Ok(())
            }
        }
    }

    /// A placeholder takes its new size, time and cTag, in place (same inode).
    /// Under the per-inode lock, which a fill holds for its whole run: if one is
    /// running, this waits for the next cycle rather than for the download.
    /// Whether it was updated now.
    fn update_placeholder(&self, file: File, row: &Row, run: &mut Run) -> Result<bool, ApplyError> {
        let key = super::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.deferred += 1;
            return Ok(false);
        };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        // Looked at again under the lock: a fill may have finished meanwhile.
        if !matches!(read_state(&writable), Ok(Some(State::OnlineOnly))) {
            run.out.deferred += 1;
            return Ok(false);
        }
        // A checkpoint of another version goes, with its bytes; one of this
        // very version stays (A-I1). An online-only file carries no ignore
        // mark (the helper marks only what reads hydrated), so the punch
        // needs no ClearIgnore (question, answered).
        let stale = placeholder::read_progress(&writable)?.is_some_and(|p| Some(p.ctag.as_str()) != row.ctag.as_deref());
        if stale {
            placeholder::remove_progress(&writable)?;
            placeholder::punch_all(&writable)?;
        }
        writable.set_len(row.size)?;
        if let Some(ctag) = &row.ctag {
            placeholder::write_ctag(&writable, ctag)?;
        }
        placeholder::set_mtime(&writable, cloud_time(row))?;
        if row.size == 0 {
            // Nothing left to fetch: an empty file is a downloaded one.
            placeholder::write_stamp(&writable)?;
            placeholder::write_state(&writable, State::Hydrated)?;
        }
        writable.sync_all()?;
        run.out.updated += 1;
        Ok(true)
    }

    /// A placeholder of the tree's very version whose time is not the
    /// cloud's — a fill wrote into it and stopped — gets the cloud's time
    /// back, and keeps its checkpoint and the bytes it counts (A-I1). KIO
    /// checks a thumbnail against the file's time, so a wrong one also had
    /// Dolphin open the file to make its own thumbnail: a download. Under
    /// the per-inode lock, as [`Self::update_placeholder`]: a fill running
    /// now leaves it for the next cycle. The owner sets a time through a
    /// read-only descriptor, lock or not.
    fn put_time_back(&self, file: File, row: &Row, run: &mut Run) -> Result<(), ApplyError> {
        let key = super::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.deferred += 1;
            return Ok(());
        };
        if !matches!(read_state(&file), Ok(Some(State::OnlineOnly))) {
            run.out.deferred += 1;
            return Ok(());
        }
        placeholder::set_mtime(&file, cloud_time(row))?;
        Ok(())
    }
}

/// The time the cloud gives an item, as a placeholder carries it.
fn cloud_time(row: &Row) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(row.mtime.max(0) as u64)
}

/// Whether a scanned entry is the temporary link `swap_in` leaves when its
/// rename over the old name never lands (see [`Materializer::discard_leftover_replacement`]):
/// its name is exactly `.konedrive-new-<id>`, and that same id is also
/// carried by another entry the scan found — the real file the replacement
/// was for. A folder waiting under this name to be placed (the ordinary,
/// single-entry case) is never a file, and its id appears nowhere else yet.
fn is_leftover_replacement(entry: &Scanned, id: &str, counts: &HashMap<&str, usize>) -> bool {
    !entry.is_dir && entry.rel.file_name() == Some(OsStr::new(&format!("{NEW_PREFIX}{id}"))) && counts.get(id).copied().unwrap_or(0) > 1
}

/// Whether deleting or replacing this file would lose something only this
/// machine has: a downloaded file changed since (its stamp does not match), or
/// a file of ours in a state nobody can vouch for that holds data.
pub(crate) fn holds_local_work(file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let blocks = file.metadata().map(|m| m.blocks()).unwrap_or(1);
    match read_state(file) {
        Ok(Some(State::Hydrated)) => {
            let empty = file.metadata().map(|m| m.len() == 0).unwrap_or(false);
            !empty && !matches!(stamp_matches(file), Ok(true))
        }
        Ok(Some(State::OnlineOnly | State::Hydrating | State::Dehydrating)) => false,
        Ok(None) | Err(_) => blocks > 0,
    }
}
#[derive(Debug)]
pub enum ReplaceOutcome {
    Replaced,
    /// Nothing to do any more: the file moved, changed, was freed up or is
    /// already this version. The next cycle looks again.
    Current,
    Failed(String),
    /// Failed for want of disk space: the disk cannot hold both versions, or
    /// filled up while the new one downloaded. Said and retried as `Failed`
    /// is; told apart so that the activity log can say exactly "not enough
    /// disk space".
    NoSpace(String),
}

/// The margin `replace` keeps free beside the new version's own bytes when
/// checking whether the disk can hold both at once: room for
/// filesystem bookkeeping and whatever else is filling or freeing up
/// concurrently, not just the new version's exact byte count.
const REPLACE_SPACE_MARGIN: u64 = 64 << 20;

/// `disk.dir(parent)` for a replacement already in flight: a folder above the
/// file having moved or been removed since is not a failure to report and
/// retry forever — it is nothing left to do here; the file
/// is wherever the tree now says it is, and the next cycle looks there. Any
/// other error is still an error.
fn replacement_dir(disk: &Disk, parent: &Path) -> std::io::Result<Option<File>> {
    match disk.dir(parent) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A file's identity strong enough to survive `old` being dropped and
/// reopened later, rather than kept open for the whole download (round 1,
/// issue 3). `(dev, ino)` alone is not enough (round 2's finding): once its
/// descriptor is closed, nothing pins the inode number — it can be freed and
/// reused by an unrelated file created while the new version downloads, and
/// the two would then compare equal. [`Fingerprint`] tells a reused inode
/// apart from the file that had it before.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    fingerprint: Fingerprint,
}

/// What distinguishes a file from another that happens to reuse its inode:
/// its birth time where the filesystem reports one (`statx`'s `btime` on
/// Linux, behind `Metadata::created()`), or — where it does not — its mtime
/// and size, which the stamp check already constrains for any file
/// `replace` considers taking as "the same old file".
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fingerprint {
    Born(SystemTime),
    Stamp { mtime: SystemTime, size: u64 },
}

impl FileIdentity {
    fn of(file: &File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let fingerprint = match meta.created() {
            Ok(created) => Fingerprint::Born(created),
            Err(_) => Fingerprint::Stamp { mtime: meta.modified()?, size: meta.len() },
        };
        Ok(Self { dev: meta.dev(), ino: meta.ino(), fingerprint })
    }
}

/// The new version of a downloaded file is fetched into a nameless
/// file beside it, verified, labelled and swapped in with one rename. Never
/// written over the old one in place — a reader would see a mix. If the disk
/// cannot hold both, the old version stays.
pub async fn replace(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement) -> ReplaceOutcome {
    match replace_inner(disk, locks, source, r).await {
        Ok(outcome) => outcome,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) => ReplaceOutcome::NoSpace(format!(
            "not enough space to finish the new version of {}; the old version stays",
            r.rel.display()
        )),
        Err(e) => ReplaceOutcome::Failed(e.to_string()),
    }
}

async fn replace_inner(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement) -> std::io::Result<ReplaceOutcome> {
    let parent = r.rel.parent().unwrap_or(Path::new(""));
    let Some(name) = r.rel.file_name() else { return Ok(ReplaceOutcome::Current) };
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    let still_there = |dir: &File| -> std::io::Result<Option<File>> {
        match disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: false } if id == r.id => {}
            _ => return Ok(None),
        }
        let old = match disk.open_file(dir, name) {
            Ok(old) => old,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let hydrated = matches!(read_state(&old), Ok(Some(State::Hydrated)));
        let other_version = placeholder::read_ctag(&old)?.as_deref() != Some(r.ctag.as_str());
        Ok((hydrated && other_version && !holds_local_work(&old)).then_some(old))
    };
    let Some(old) = still_there(&dir)? else { return Ok(ReplaceOutcome::Current) };
    // Kept only as identity from here, not as a hold on the file: a Free up
    // space must be able to take the old file's write lease while the new
    // version downloads, which it could not while `old` stayed open for the
    // whole download.
    let old_identity = FileIdentity::of(&old)?;
    let old_key = super::InodeKey::of(&old)?;
    drop(old);

    // Both versions must fit at once; checked before anything is
    // downloaded, so that a full disk costs no bandwidth on every retry.
    let fs = nix::sys::statvfs::fstatvfs(&dir)?;
    let free = fs.blocks_available() as u64 * fs.fragment_size() as u64;
    if free < r.size.saturating_add(REPLACE_SPACE_MARGIN) {
        return Ok(ReplaceOutcome::NoSpace(format!(
            "not enough space to download the new version of {} beside the old one; the old version stays",
            r.rel.display()
        )));
    }

    let new = disk.tmpfile(&dir)?;
    placeholder::write_item_id(&new, &r.id)?;
    let downloaded = match super::source::download_into(&new, &r.id, source).await {
        Ok(downloaded) => downloaded,
        Err(errno) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
            return Ok(ReplaceOutcome::NoSpace(format!(
                "not enough space to download the new version of {}; the old version stays",
                r.rel.display()
            )))
        }
        Err(errno) => {
            return Ok(ReplaceOutcome::Failed(format!(
                "the new version of {} could not be downloaded ({}); the old version stays",
                r.rel.display(),
                std::io::Error::from_raw_os_error(errno)
            )))
        }
    };
    new.set_len(downloaded.size)?;
    if let Err(e) = placeholder::set_mtime(&new, downloaded.mtime) {
        tracing::warn!("{}: cannot apply the new version's mtime: {e}", r.rel.display());
    }
    new.sync_data()?;
    if let Some(version) = &downloaded.version {
        placeholder::write_ctag(&new, &version.ctag)?;
    }
    placeholder::write_stamp(&new)?;
    placeholder::write_state(&new, State::Hydrated)?;
    placeholder::set_mode(&new, if disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE })?;
    new.sync_all()?;

    // The swap, under the old file's lock, after looking again: a Free up
    // space, a fill or a local edit may have happened while this downloaded.
    let _guard = locks.lock(old_key).await;
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    let now = still_there(&dir)?;
    let same_file = match &now {
        Some(now) => FileIdentity::of(now)? == old_identity,
        None => false,
    };
    if !same_file {
        return Ok(ReplaceOutcome::Current);
    }
    // A pin of the file's own goes with it to the new version, which is
    // another inode.
    if let Some(now) = &now {
        if placeholder::read_pin(now)? {
            placeholder::write_pin(&new)?;
        }
    }
    let temp = format!("{NEW_PREFIX}{}", r.id);
    clear_leftover_link(disk, &dir, OsStr::new(&temp), &r.id)?;
    disk.swap_in(&dir, &new, OsStr::new(&temp), name)?;
    Ok(ReplaceOutcome::Replaced)
}

/// Removes the temporary link an earlier swap of the same file left — a
/// crash between its link and its rename — which made every later swap fail
/// `EEXIST` until a Full reconcile cleared it. Only
/// a plain file carrying this very item id and no local work goes; anything
/// else is left, and the swap fails as before. A Full reconcile rescues one
/// that holds work (`discard_leftover_replacement`).
fn clear_leftover_link(disk: &Disk, dir: &File, temp: &OsStr, id: &str) -> std::io::Result<()> {
    if let Probe::Managed { id: found, is_dir: false } = disk.probe(dir, temp)? {
        if found == id && !holds_local_work(&disk.open_file(dir, temp)?) {
            tracing::info!("clearing the temporary link {} an earlier replacement left", temp.to_string_lossy());
            disk.remove(dir, temp, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    use konedrive_fs::placeholder::{write_stamp, write_state, State, XATTR_ROOT};
    use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
    use nix::sys::socket::{accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
    use xattr::FileExt;

    use super::*;
    use crate::sync::root::SyncRoot;
    use crate::tree::{Change, TreeStore};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: SyncRoot,
        store: Store,
        rescue: tempfile::TempDir,
        /// `None` in an async test: the `Materializer`'s handle then comes
        /// from `Handle::current()`, since there is already a runtime here.
        runtime: Option<tokio::runtime::Runtime>,
    }

    fn build_fixture(runtime: Option<tokio::runtime::Runtime>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let root_id = "8f6c0a3e-3b0e-4d7a-9c1e-5b2d7e4f1a90".to_owned();
        File::open(&path).unwrap().set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
        Fixture {
            _dir: dir,
            root: SyncRoot { path, root_id },
            store: Store::new(TreeStore::in_memory().unwrap()),
            rescue: tempfile::tempdir().unwrap(),
            runtime,
        }
    }

    fn fixture() -> Fixture {
        build_fixture(Some(tokio::runtime::Runtime::new().unwrap()))
    }

    /// A fixture for an already-`async` test: no `Runtime` of its own.
    fn fixture_async() -> Fixture {
        build_fixture(None)
    }

    fn row(id: &str, parent: &str, name: &str, kind: Kind, size: u64) -> Row {
        Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind, size, mtime: 1_700_000_000, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed }
    }

    fn root_row() -> Change {
        Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
    }

    fn up(row: Row) -> Change {
        Change::Upsert(row)
    }

    fn folder(id: &str, parent: &str, name: &str) -> Change {
        up(row(id, parent, name, Kind::Folder, 0))
    }

    fn file(id: &str, parent: &str, name: &str) -> Change {
        up(row(id, parent, name, Kind::File, 4096))
    }

    /// A locked tree cannot be deleted by the temporary directory's cleanup.
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(disk) = Disk::open(&self.root, false) {
                let _ = disk.unlock_tree();
            }
        }
    }

    impl Fixture {
        /// The handle a `Materializer` waits on the helper through: this
        /// fixture's own runtime, or — in an async test, which has none of
        /// its own — the one already running it.
        fn handle(&self) -> tokio::runtime::Handle {
            match &self.runtime {
                Some(rt) => rt.handle().clone(),
                None => tokio::runtime::Handle::current(),
            }
        }

        fn materializer(&self, locked: bool, link: Option<HelperLink>) -> Materializer {
            Materializer {
                disk: Disk::open(&self.root, locked).unwrap(),
                store: self.store.clone(),
                link,
                runtime: self.handle(),
                locks: InodeLocks::new(),
                root_item_id: "R".into(),
                rescue_into: self.rescue.path().join("now"),
                cancel: CancellationToken::new(),
            }
        }

        /// A full listing of `changes`, reconciled and committed.
        fn listed(&self, changes: &[Change], locked: bool) -> Applied {
            self.store.with(|s| { s.begin_staging(false)?; s.stage(changes) }).unwrap();
            let applied = self.materializer(locked, None).apply(Scope::Full).unwrap();
            self.store.with(|s| s.commit_staging("link-1")).unwrap();
            applied
        }

        /// A delta on top of what is committed, reconciled in the Changed scope.
        fn delta(&self, changes: &[Change], locked: bool) -> Result<Applied, ApplyError> {
            self.store.with(|s| { s.begin_staging(true)?; s.stage(changes) }).unwrap();
            let ids = changes.iter().map(|c| c.id().to_owned()).collect();
            self.materializer(locked, None).apply(Scope::Changed(ids))
        }

        /// [`Self::listed`] with `tree()`, from an async test: the reconcile
        /// itself runs on a blocking thread, since it may wait on the
        /// runtime it is itself running on (`Materializer::mark`).
        async fn listed_async(&self, locked: bool) -> Applied {
            self.store.with(|s| { s.begin_staging(false)?; s.stage(&tree()) }).unwrap();
            let m = self.materializer(locked, None);
            let applied = tokio::task::spawn_blocking(move || m.apply(Scope::Full)).await.unwrap().unwrap();
            self.store.with(|s| s.commit_staging("link-1")).unwrap();
            applied
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.path.join(rel)
        }
    }

    fn id_at(path: &Path) -> Option<String> {
        xattr::get(path, "user.konedrive.item-id").unwrap().map(|v| String::from_utf8(v).unwrap())
    }

    fn ino(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).unwrap().ino()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn tree() -> Vec<Change> {
        vec![root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt"), folder("E", "D", "deep"), file("G", "E", "g.txt"), file("T", "R", "top.bin")]
    }

    /// Round 2: `(dev, ino)` alone is not a strong enough identity for a file
    /// that was dropped and reopened later — an inode can be freed and
    /// reused by an unrelated file in between. `FileIdentity` must tell that
    /// case apart, which an ino-only comparison cannot.
    #[test]
    fn a_reused_inode_with_a_different_birth_time_is_a_different_file() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500);
        let a = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
        let same = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
        let reused = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t2) };
        assert_eq!(a, same, "the same dev, ino and birth time is the same file");
        assert_ne!(a, reused, "the same ino with a different birth time is a different file");
    }

    use std::os::unix::fs::FileExt as _;

    use async_trait::async_trait;
    use konedrive_fs::placeholder::{read_ctag, read_progress, write_ctag, write_progress, Progress};

    use crate::quickxor::QuickXor;
    use crate::sync::source::{ContentSource, Fetched, SourceError, Version};

    /// Downloads a file the way a finished fill leaves it: content, cTag, stamp.
    fn hydrate_by_hand(path: &Path, content: &[u8], ctag: &str) {
        let file = konedrive_fs::placeholder::reopen_writable(&File::open(path).unwrap()).unwrap();
        file.set_len(0).unwrap();
        file.write_all_at(content, 0).unwrap();
        write_ctag(&file, ctag).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();
    }

    fn changed(id: &str, parent: &str, name: &str, size: u64, ctag: &str) -> Change {
        let mut r = row(id, parent, name, Kind::File, size);
        r.ctag = Some(ctag.into());
        r.mtime = 1_700_000_500;
        up(r)
    }

    /// a new folder's temporary directory left with
    /// no id — killed between `mkdirat` and its label — made every later
    /// reconcile fail `EEXIST`, for good. An empty one is cleared and the
    /// folder made; one with something in it is rescued first.
    #[test]
    fn a_new_folders_temporary_directory_left_without_its_id_is_cleared() {
        let f = fixture();
        std::fs::create_dir(f.path(".konedrive-new-D")).unwrap();
        f.listed(&[root_row(), folder("D", "R", "docs")], true);
        assert_eq!(id_at(&f.path("docs")).as_deref(), Some("D"));
        assert!(!f.path(".konedrive-new-D").exists());

        let docs = File::open(f.path("docs")).unwrap();
        placeholder::with_owner_write(&docs, || std::fs::create_dir(f.path("docs/.konedrive-new-E"))).unwrap();
        std::fs::write(f.path("docs/.konedrive-new-E/mine.txt"), b"mine").unwrap();
        let applied = f.delta(&[folder("E", "D", "deep")], true).unwrap();
        assert_eq!(id_at(&f.path("docs/deep")).as_deref(), Some("E"));
        assert_eq!(applied.rescued.len(), 1, "{:?}", applied.rescued);
        assert_eq!(std::fs::read(applied.rescued[0].rescued.join("mine.txt")).unwrap(), b"mine");
    }

    /// a directory of the user's own in the way,
    /// holding a placeholder of ours, is rescued with the user's files — and
    /// without the placeholder, which stripped of its state read as a file
    /// of zeros in the rescue directory.
    #[test]
    fn a_rescued_directory_keeps_the_users_files_and_not_our_placeholders() {
        let f = fixture();
        f.listed(&[root_row()], false);
        std::fs::create_dir(f.path("docs")).unwrap();
        std::fs::write(f.path("docs/mine.txt"), b"mine").unwrap();
        let docs = File::open(f.path("docs")).unwrap();
        placeholder::create_placeholder(&docs, "cloud.bin", "X", 4096, SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)).unwrap();

        let applied = f.delta(&[folder("D", "R", "docs")], false).unwrap();

        let rescued = &applied.rescued[0].rescued;
        assert_eq!(std::fs::read(rescued.join("mine.txt")).unwrap(), b"mine");
        assert!(!rescued.join("cloud.bin").exists(), "a placeholder would read as zeros there");
        assert_eq!(id_at(&f.path("docs")).as_deref(), Some("D"));
    }

    #[test]
    fn a_placeholder_changed_in_the_cloud_is_updated_in_place() {
        let f = fixture();
        f.listed(&tree(), true);
        let path = f.path("docs/f.txt");
        let before = ino(&path);
        let applied = f.delta(&[changed("F", "D", "f.txt", 8192, "c2")], true).unwrap();
        assert_eq!(applied.updated, 1);
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!((meta.ino(), meta.len(), meta.mtime()), (before, 8192, 1_700_000_500));
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
        assert_eq!(mode(&path), 0o444);
    }

    #[test]
    fn a_checkpoint_of_the_old_version_goes_with_it() {
        let f = fixture();
        f.listed(&tree(), false);
        let path = f.path("docs/f.txt");
        {
            let file = File::options().read(true).write(true).open(&path).unwrap();
            file.write_all_at(&[5u8; 2048], 0).unwrap();
            write_progress(&file, &Progress { ctag: "c-F".into(), bytes: 2048 }).unwrap();
        }
        f.delta(&[changed("F", "D", "f.txt", 4096, "c2")], false).unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(read_progress(&file).unwrap(), None);
        assert!(std::fs::read(&path).unwrap().iter().all(|b| *b == 0), "the old version's bytes are gone");
    }

    #[test]
    fn a_placeholder_emptied_in_the_cloud_becomes_an_empty_downloaded_file() {
        let f = fixture();
        f.listed(&tree(), false);
        f.delta(&[changed("F", "D", "f.txt", 0, "c2")], false).unwrap();
        let file = File::open(f.path("docs/f.txt")).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
        assert!(stamp_matches(&file).unwrap());
    }

    #[test]
    fn a_downloaded_file_of_the_same_version_is_left_alone() {
        let f = fixture();
        f.listed(&tree(), false);
        hydrate_by_hand(&f.path("docs/f.txt"), b"content", "c-F");
        let mut same = row("F", "D", "f.txt", Kind::File, 7);
        same.mtime = 1_700_000_900; // metadata changed, content did not
        let applied = f.delta(&[up(same)], false).unwrap();
        assert!(applied.replacements.is_empty());
        assert_eq!(std::fs::read(f.path("docs/f.txt")).unwrap(), b"content");
    }

    #[test]
    fn a_downloaded_file_changed_in_the_cloud_is_queued_for_replacement() {
        let f = fixture();
        f.listed(&tree(), false);
        hydrate_by_hand(&f.path("docs/f.txt"), b"content", "c-F");
        let applied = f.delta(&[changed("F", "D", "f.txt", 9, "c2")], false).unwrap();
        assert_eq!(applied.replacements, vec![Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 9 }]);
        assert_eq!(std::fs::read(f.path("docs/f.txt")).unwrap(), b"content", "untouched until replaced");
    }

    #[test]
    fn a_file_changed_here_and_in_the_cloud_is_rescued_and_shown_as_the_new_version() {
        let f = fixture();
        f.listed(&tree(), false);
        let path = f.path("docs/f.txt");
        hydrate_by_hand(&path, b"content", "c-F");
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all_at(b" and mine", 7).unwrap();
        let applied = f.delta(&[changed("F", "D", "f.txt", 9, "c2")], false).unwrap();
        assert_eq!(applied.rescued.len(), 1);
        assert_eq!(applied.rescued[0].original, PathBuf::from("docs/f.txt"), "where it was, for the conflict");
        assert_eq!(std::fs::read(&applied.rescued[0].rescued).unwrap(), b"content and mine");
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    }

    /// An incremental cycle says what it did item by item —
    /// added, updated, moved (and from where), removed — and a folder removed
    /// with everything in it is one removal, not one per file.
    #[test]
    fn a_changed_scope_notes_each_item_it_changed() {
        let f = fixture();
        f.listed(&tree(), false);
        let applied = f
            .delta(
                &[
                    file("N", "R", "new.txt"),
                    changed("F", "D", "f.txt", 9, "c2"),
                    file("T", "R", "renamed.bin"),
                    Change::Delete("E".into()),
                ],
                false,
            )
            .unwrap();
        let mut changes = applied.changes.clone();
        changes.sort_by(|a, b| a.rel.cmp(&b.rel));
        let change = |kind, rel: &str, from: Option<&str>| Changed { kind, rel: rel.into(), from: from.map(PathBuf::from) };
        assert_eq!(
            changes,
            vec![
                change(EventKind::Removed, "docs/deep", None),
                change(EventKind::Updated, "docs/f.txt", None),
                change(EventKind::Added, "new.txt", None),
                change(EventKind::Moved, "renamed.bin", Some("top.bin")),
            ]
        );
    }

    #[test]
    fn a_file_being_filled_is_left_for_the_next_cycle() {
        let f = fixture();
        f.listed(&tree(), false);
        let path = f.path("docs/f.txt");
        let m = f.materializer(false, None);
        let _held = m.locks.try_lock(crate::sync::InodeKey::of(&File::open(&path).unwrap()).unwrap()).unwrap();
        f.store.with(|s| { s.begin_staging(true)?; s.stage(&[changed("F", "D", "f.txt", 8192, "c2")]) }).unwrap();
        let applied = m.apply(Scope::Changed(vec!["F".into()])).unwrap();
        assert_eq!((applied.updated, applied.deferred), (0, 1));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
    }

    /// Serves `content` as version `ctag`, with its hash; `on_fetch` runs first.
    /// `damaged` flips one byte of what it streams, not of what it hashes.
    struct Memory {
        ctag: String,
        content: Vec<u8>,
        damaged: bool,
        on_fetch: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Memory {
        fn new(ctag: &str, content: &[u8]) -> Self {
            Self { ctag: ctag.into(), content: content.to_vec(), damaged: false, on_fetch: std::sync::Mutex::new(None) }
        }
    }

    #[async_trait]
    impl ContentSource for Memory {
        async fn fetch(&self, _item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            if let Some(hook) = self.on_fetch.lock().unwrap().take() {
                hook();
            }
            let mut hash = QuickXor::new();
            hash.update(&self.content);
            let mut served = self.content.clone();
            if self.damaged {
                served[0] ^= 1;
            }
            let start = (from as usize).min(served.len());
            Ok(Fetched {
                served_from: from,
                size: self.content.len() as u64,
                mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500),
                version: Some(Version { ctag: self.ctag.clone(), quick_xor: Some(hash.finish()) }),
                stream: Box::new(std::io::Cursor::new(served[start..].to_vec())),
            })
        }
    }

    #[tokio::test]
    async fn a_replacement_swaps_in_the_new_version_and_a_reader_keeps_the_old() {
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, true).unwrap(), f.path("docs/f.txt"));
        f.listed_async(true).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let reader = File::open(&path).unwrap();
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let outcome = replace(&disk, &InodeLocks::new(), &Memory::new("c2", b"the new version"), &replacement).await;
        assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"the new version");
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
        assert!(stamp_matches(&file).unwrap());
        assert_eq!(mode(&path), 0o444);
        let mut old = vec![0u8; 11];
        reader.read_exact_at(&mut old, 0).unwrap();
        assert_eq!(&old, b"old version", "a reader of the old file keeps it");
    }

    #[tokio::test]
    async fn a_replacement_that_does_not_match_its_hash_leaves_the_old_version() {
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
        f.listed_async(false).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let before = ino(&path);
        let damaged = Memory { damaged: true, ..Memory::new("c2", b"the new version") };
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let outcome = replace(&disk, &InodeLocks::new(), &damaged, &replacement).await;
        assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
        assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
    }

    #[tokio::test]
    async fn a_replacement_that_cannot_be_downloaded_leaves_the_old_version() {
        struct Gone;
        #[async_trait]
        impl ContentSource for Gone {
            async fn fetch(&self, _: &str, _: u64) -> Result<Fetched, SourceError> {
                Err(SourceError::NotFound("gone".into()))
            }
        }
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
        f.listed_async(false).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let before = ino(&path);
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let outcome = replace(&disk, &InodeLocks::new(), &Gone, &replacement).await;
        assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
        assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
    }

    #[tokio::test]
    async fn a_file_freed_up_while_its_replacement_downloaded_is_left_as_it_is() {
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
        f.listed_async(false).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let before = ino(&path);
        let source = Memory::new("c2", b"the new version");
        let freed = path.clone();
        *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
            let file = File::options().read(true).write(true).open(&freed).unwrap();
            // `old` must not still be open here, or Free up
            // space could not take this file's write lease while its
            // replacement downloads.
            assert!(
                konedrive_fs::lease::WriteLease::take(&file).unwrap().is_some(),
                "the old file's write lease is free while its replacement downloads"
            );
            write_state(&file, State::OnlineOnly).unwrap();
        }));
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
        assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
        assert_eq!(ino(&path), before, "the user freed it up; it is not filled behind their back");
    }

    /// A folder above the file moves (or is removed) while
    /// its replacement downloads. `disk.dir(parent)` then answers ENOENT —
    /// the same "nothing to do any more, the next cycle looks again" case as
    /// any other change underneath the replacement, not a download failure to
    /// report and keep retrying forever.
    #[tokio::test]
    async fn a_folder_moved_while_its_replacement_downloaded_is_left_as_it_is() {
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
        f.listed_async(false).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let before = ino(&path);
        let source = Memory::new("c2", b"the new version");
        let root = f.root.path.clone();
        *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
            std::fs::rename(root.join("docs"), root.join("papers")).unwrap();
        }));
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
        assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
        assert_eq!(ino(&f.path("papers/f.txt")), before, "the file is untouched at its new path");
        assert_eq!(std::fs::read(f.path("papers/f.txt")).unwrap(), b"old version");
    }

    /// The swap under the old file's lock really does
    /// wait for it — untested until now — rather than racing whoever holds
    /// it (a fill, a Free up, another replacement of the same file).
    #[tokio::test]
    async fn a_replacements_swap_waits_for_the_per_inode_lock() {
        let f = fixture_async();
        let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
        f.listed_async(false).await;
        hydrate_by_hand(&path, b"old version", "c-F");
        let locks = InodeLocks::new();
        let key = crate::sync::InodeKey::of(&File::open(&path).unwrap()).unwrap();
        let held = locks.lock(key).await;

        let task_locks = locks.clone();
        let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
        let handle = tokio::spawn(async move {
            let source = Memory::new("c2", b"the new version");
            replace(&disk, &task_locks, &source, &replacement).await
        });

        // The download itself is instant (an in-memory source, no delay);
        // this is time enough for the task to reach the lock and block on it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_finished(), "the swap has not gone ahead while the lock is held");
        assert_eq!(std::fs::read(&path).unwrap(), b"old version", "not swapped in yet");

        drop(held);
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"the new version", "swapped in once the lock is free");
    }

    /// A file mid-fill (`Hydrating`) is left exactly
    /// alone by `check_file` — untested until now — deferred like a file
    /// being freed up, never touched.
    #[test]
    fn a_file_hydrating_right_now_is_left_for_the_next_cycle() {
        let f = fixture();
        f.listed(&tree(), false);
        let path = f.path("docs/f.txt");
        {
            let file = File::options().read(true).write(true).open(&path).unwrap();
            write_state(&file, State::Hydrating).unwrap();
        }
        let applied = f.delta(&[changed("F", "D", "f.txt", 8192, "c2")], false).unwrap();
        assert_eq!((applied.updated, applied.deferred), (0, 1));
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrating));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096, "untouched");
    }

    #[test]
    fn a_full_reconcile_builds_the_tree_from_nothing() {
        let f = fixture();
        let applied = f.listed(&tree(), false);
        assert_eq!(applied.created, 5);
        for (rel, id) in [("docs", "D"), ("docs/f.txt", "F"), ("docs/deep", "E"), ("docs/deep/g.txt", "G"), ("top.bin", "T")] {
            assert_eq!(id_at(&f.path(rel)).as_deref(), Some(id), "{rel}");
        }
        let meta = std::fs::metadata(f.path("docs/f.txt")).unwrap();
        assert_eq!((meta.len(), meta.mtime()), (4096, 1_700_000_000));
        assert!(!f.path(".konedrive-holding").exists());
        assert_eq!(mode(&f.path("docs/f.txt")), 0o644, "no lock on an unlocked folder");
    }

    #[test]
    fn under_the_lock_everything_ends_read_only() {
        let f = fixture();
        f.listed(&tree(), true);
        for rel in ["", "docs", "docs/deep"] {
            assert_eq!(mode(&f.path(rel)), 0o555, "{rel:?}");
        }
        for rel in ["docs/f.txt", "top.bin", "docs/deep/g.txt"] {
            assert_eq!(mode(&f.path(rel)), 0o444, "{rel}");
        }
        let refused = std::fs::write(f.path("docs/new.txt"), b"x").unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_rename_keeps_the_inode() {
        let f = fixture();
        f.listed(&tree(), true);
        let before = ino(&f.path("docs/f.txt"));
        f.delta(&[file("F", "D", "renamed.txt")], true).unwrap();
        assert_eq!(ino(&f.path("docs/renamed.txt")), before);
        assert!(!f.path("docs/f.txt").exists());
    }

    #[test]
    fn a_moved_folder_takes_its_contents_along() {
        let f = fixture();
        f.listed(&tree(), true);
        let before = ino(&f.path("docs/deep/g.txt"));
        f.delta(&[folder("E", "R", "moved")], true).unwrap();
        assert_eq!(ino(&f.path("moved/g.txt")), before);
        assert_eq!(id_at(&f.path("moved")).as_deref(), Some("E"));
        assert!(!f.path("docs/deep").exists());
        assert_eq!(mode(&f.path("moved")), 0o555);
    }

    /// Phase 1 goes deepest first: when a folder and something inside it both
    /// move, the inner one leaves before the folder changes its path.
    #[test]
    fn a_folder_and_a_file_inside_it_move_in_one_delta() {
        let f = fixture();
        f.listed(&tree(), true);
        let before = ino(&f.path("docs/deep/g.txt"));
        f.delta(&[folder("E", "R", "moved"), file("G", "R", "g.txt")], true).unwrap();
        assert_eq!(ino(&f.path("g.txt")), before);
        assert_eq!(id_at(&f.path("moved")).as_deref(), Some("E"));
        assert!(!f.path("docs/deep").exists());
    }

    /// The same order in a Full reconcile, which finds the misplaced items by
    /// scanning.
    #[test]
    fn a_full_reconcile_moves_the_inner_of_two_misplaced_items_first() {
        let f = fixture();
        f.listed(&tree(), false);
        let before = ino(&f.path("docs/deep/g.txt"));
        f.store.with(|s| { s.begin_staging(true)?; s.stage(&[folder("E", "R", "deep"), file("G", "E", "g2.txt")]) }).unwrap();
        f.materializer(false, None).apply(Scope::Full).unwrap();
        assert_eq!(ino(&f.path("deep/g2.txt")), before);
        assert!(!f.path("docs/deep").exists());
    }

    #[test]
    fn two_names_swapped_end_up_swapped() {
        let f = fixture();
        f.listed(&[root_row(), file("A", "R", "a"), file("B", "R", "b")], true);
        let (a, b) = (ino(&f.path("a")), ino(&f.path("b")));
        let applied = f.delta(&[file("A", "R", "b"), file("B", "R", "a")], true).unwrap();
        assert_eq!((ino(&f.path("b")), ino(&f.path("a"))), (a, b));
        assert!(applied.rescued.is_empty());
        assert!(!f.path(".konedrive-holding").exists());
    }

    #[test]
    fn a_cycle_of_three_resolves() {
        let f = fixture();
        f.listed(&[root_row(), file("A", "R", "a"), file("B", "R", "b"), file("C", "R", "c")], false);
        let (a, b, c) = (ino(&f.path("a")), ino(&f.path("b")), ino(&f.path("c")));
        f.delta(&[file("A", "R", "b"), file("B", "R", "c"), file("C", "R", "a")], false).unwrap();
        assert_eq!((ino(&f.path("b")), ino(&f.path("c")), ino(&f.path("a"))), (a, b, c));
    }

    #[test]
    fn a_deleted_folder_goes_but_a_file_changed_here_is_rescued() {
        let f = fixture();
        f.listed(&tree(), true);
        // f.txt was downloaded, then written to through a descriptor someone
        // opened during a lock window.
        let path = f.path("docs/f.txt");
        {
            let file = konedrive_fs::placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
            std::os::unix::fs::FileExt::write_all_at(&file, &[9u8; 4096], 0).unwrap();
            write_state(&file, State::Hydrated).unwrap();
            write_stamp(&file).unwrap();
            // Appended, so the size changes: an mtime can land in the same
            // clock tick as the stamp.
            std::os::unix::fs::FileExt::write_all_at(&file, b"local work", 4096).unwrap();
        }
        let applied = f.delta(&[Change::Delete("D".into())], true).unwrap();
        assert!(!f.path("docs").exists());
        assert_eq!(
            applied.rescued,
            vec![Rescued { original: "docs/f.txt".into(), rescued: f.rescue.path().join("now/docs/f.txt") }]
        );
        let kept = &applied.rescued[0].rescued;
        assert!(std::fs::read(kept).unwrap().ends_with(b"local work"));
        assert_eq!(mode(kept), 0o644);
        let names: Vec<_> = xattr::list(kept).unwrap().collect();
        assert!(names.iter().all(|n| !n.to_string_lossy().starts_with("user.konedrive.")), "{names:?}");
    }

    #[test]
    fn a_name_too_long_is_not_created_and_a_folder_renamed_to_one_leaves() {
        let f = fixture();
        let long = "я".repeat(128);
        let mut skipped = row("L", "R", &long, Kind::File, 1);
        skipped.placement = Placement::Skipped(crate::tree::SkipReason::NameTooLong);
        f.listed(&[root_row(), up(skipped), folder("D", "R", "docs"), file("F", "D", "f.txt")], true);
        assert_eq!(std::fs::read_dir(&f.root.path).unwrap().count(), 1, "only docs");
        let mut renamed = row("D", "R", &long, Kind::Folder, 0);
        renamed.placement = Placement::Skipped(crate::tree::SkipReason::NameTooLong);
        f.delta(&[up(renamed)], true).unwrap();
        assert!(!f.path("docs").exists(), "a folder that can no longer be shown is removed; its clean files are in the cloud");
    }

    #[test]
    fn the_order_of_the_rows_does_not_matter() {
        let f = fixture();
        let mut changes = tree();
        changes.reverse();
        f.listed(&changes, false);
        assert_eq!(id_at(&f.path("docs/deep/g.txt")).as_deref(), Some("G"));
    }

    #[test]
    fn a_folder_that_does_not_match_the_stored_tree_needs_a_full_reconcile() {
        let f = fixture();
        f.listed(&tree(), false);
        std::fs::remove_file(f.path("docs/f.txt")).unwrap();
        let err = f.delta(&[file("F", "D", "renamed.txt")], false).unwrap_err();
        assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
        f.materializer(false, None).apply(Scope::Full).unwrap();
        assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
    }

    /// A stranger directory where a new folder belongs leaves whole, by one
    /// rename, even locked and read-only itself; it arrives as the user's own.
    #[test]
    fn a_stranger_folder_in_the_way_is_rescued_whole_under_the_lock() {
        let f = fixture();
        f.listed(&tree(), true);
        std::fs::set_permissions(&f.root.path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir(f.path("incoming")).unwrap();
        std::fs::write(f.path("incoming/mine.txt"), b"mine").unwrap();
        std::fs::set_permissions(f.path("incoming"), std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&f.root.path, std::fs::Permissions::from_mode(0o555)).unwrap();
        let applied = f.delta(&[folder("N", "R", "incoming")], true).unwrap();
        assert_eq!(applied.rescued, vec![Rescued { original: "incoming".into(), rescued: f.rescue.path().join("now/incoming") }]);
        assert_eq!(std::fs::read(f.rescue.path().join("now/incoming/mine.txt")).unwrap(), b"mine");
        assert_eq!(mode(&f.rescue.path().join("now/incoming")), 0o755);
        assert_eq!(id_at(&f.path("incoming")).as_deref(), Some("N"));
        assert_eq!(mode(&f.root.path), 0o555);
    }

    /// The Changed scope puts an item only into a folder it has checked is
    /// ours: a folder swapped for a stranger of the same name (the lock
    /// bypassed) hands over to a Full reconcile, and nothing is made in it.
    #[test]
    fn a_changed_delta_does_not_place_into_a_folder_that_is_not_ours() {
        let f = fixture();
        f.listed(&tree(), false);
        std::fs::rename(f.path("docs/deep"), f.path("elsewhere")).unwrap();
        std::fs::create_dir(f.path("docs/deep")).unwrap();
        let err = f.delta(&[file("N", "E", "new.txt")], false).unwrap_err();
        assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
        assert!(!f.path("docs/deep/new.txt").exists());
    }

    /// A Changed run that finds a holding directory left by an earlier run
    /// hands over to a Full reconcile rather than drain what it did not put
    /// there — here a file still in the tree.
    #[test]
    fn a_changed_run_that_finds_a_holding_directory_needs_a_full_reconcile() {
        let f = fixture();
        f.listed(&tree(), false);
        std::fs::create_dir(f.path(".konedrive-holding")).unwrap();
        std::fs::rename(f.path("top.bin"), f.path(".konedrive-holding/T")).unwrap();
        let err = f.delta(&[file("F", "D", "renamed.txt")], false).unwrap_err();
        assert!(matches!(err, ApplyError::NeedFull(_)), "{err:?}");
        assert_eq!(id_at(&f.path(".konedrive-holding/T")).as_deref(), Some("T"), "nothing drained");
        f.materializer(false, None).apply(Scope::Full).unwrap();
        assert_eq!(id_at(&f.path("top.bin")).as_deref(), Some("T"));
        assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
        assert!(!f.path(".konedrive-holding").exists());
    }

    #[test]
    fn a_full_reconcile_repairs_whatever_it_finds() {
        let f = fixture();
        f.listed(&tree(), false);
        // A file of ours in the wrong folder, a stranger where a new item
        // belongs, and a folder a crash left under its temporary name.
        std::fs::rename(f.path("docs/f.txt"), f.path("f-in-the-wrong-place")).unwrap();
        std::fs::write(f.path("docs/new.txt"), b"mine").unwrap();
        std::fs::rename(f.path("docs/deep"), f.path("docs/.konedrive-new-E")).unwrap();
        f.store.with(|s| { s.begin_staging(true)?; s.stage(&[file("N", "D", "new.txt")]) }).unwrap();
        let applied = f.materializer(false, None).apply(Scope::Full).unwrap();
        assert_eq!(id_at(&f.path("docs/f.txt")).as_deref(), Some("F"));
        assert_eq!(id_at(&f.path("docs/deep")).as_deref(), Some("E"));
        assert_eq!(id_at(&f.path("docs/new.txt")).as_deref(), Some("N"));
        assert_eq!(
            applied.rescued,
            vec![Rescued { original: "docs/new.txt".into(), rescued: f.rescue.path().join("now/docs/new.txt") }]
        );
        assert_eq!(std::fs::read(&applied.rescued[0].rescued).unwrap(), b"mine");
        assert!(applied.changes.is_empty(), "a Full reconcile is one listed event, not one per item");
        assert!(!f.path("f-in-the-wrong-place").exists());
    }

    /// What `swap_in` leaves when a crash — or a rename
    /// that fails after its `linkat` succeeded — lands before the file it
    /// downloaded ever lands on the old name: a second name for the same
    /// item id, carrying the new version, that a Full reconcile must not try
    /// to send to holding alongside the real (still current) file.
    #[test]
    fn a_replacement_link_left_by_a_crashed_swap_is_discarded_and_the_real_file_still_moves() {
        let f = fixture();
        f.listed(&tree(), false);
        let disk = Disk::open(&f.root, false).unwrap();
        let dir = disk.dir(Path::new("docs")).unwrap();
        let leftover = disk.tmpfile(&dir).unwrap();
        placeholder::write_item_id(&leftover, "F").unwrap();
        leftover.write_all_at(b"the new version", 0).unwrap();
        placeholder::write_ctag(&leftover, "c2").unwrap();
        placeholder::write_state(&leftover, State::Hydrated).unwrap();
        placeholder::write_stamp(&leftover).unwrap();
        nix::unistd::linkat(leftover.as_fd(), "", dir.as_fd(), OsStr::new(".konedrive-new-F"), nix::fcntl::AtFlags::AT_EMPTY_PATH).unwrap();
        drop(leftover);
        assert!(f.path("docs/.konedrive-new-F").exists());

        f.store.with(|s| { s.begin_staging(true)?; s.stage(&[file("F", "D", "renamed.txt")]) }).unwrap();
        let applied = f.materializer(false, None).apply(Scope::Full).unwrap();
        assert!(!f.path("docs/.konedrive-new-F").exists(), "the leftover is gone");
        assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
        assert!(!f.path("docs/f.txt").exists());
        assert!(applied.rescued.is_empty(), "nothing here held local work");
    }

    #[test]
    fn a_cancelled_reconcile_stops() {
        let f = fixture();
        f.store.with(|s| { s.begin_staging(false)?; s.stage(&tree()) }).unwrap();
        let m = f.materializer(false, None);
        m.cancel.cancel();
        assert!(matches!(m.apply(Scope::Full), Err(ApplyError::Cancelled)));
    }

    /// Invariant M1: a new folder is marked before anything is created in it.
    #[test]
    fn a_new_folder_is_marked_while_it_is_still_empty() {
        let f = fixture();
        let sockets = tempfile::tempdir().unwrap();
        let socket = sockets.path().join("helper.sock");
        let marks = marking_helper(socket.clone());
        let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
        f.store.with(|s| { s.begin_staging(false)?; s.stage(&tree()) }).unwrap();
        f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
        let seen: Vec<Marked> = marks.try_iter().collect();
        assert_eq!(seen.len(), 2, "docs and docs/deep were marked");
        assert!(seen.iter().all(|m| m.entries == 0), "entries at the time of marking: {seen:?}");
        let mut names: Vec<&str> = seen.iter().map(|m| m.name.as_str()).collect();
        names.sort();
        assert_eq!(names, [".konedrive-new-D", ".konedrive-new-E"], "marked before the real name shows the folder");
    }

    /// Invariant M1 across a failure: a folder whose marking failed is left
    /// under its temporary name, and the Full reconcile that later places it
    /// marks it before its real name shows it and before anything is put in it.
    #[test]
    fn a_folder_whose_marking_failed_is_marked_when_it_is_placed_later() {
        let f = fixture();
        let sockets = tempfile::tempdir().unwrap();
        let refusing = sockets.path().join("refusing.sock");
        let _refused = helper_answering(refusing.clone(), libc::EIO);
        let link = f.handle().block_on(HelperLink::connect(&refusing)).unwrap().0;
        f.store.with(|s| { s.begin_staging(false)?; s.stage(&[root_row(), folder("D", "R", "docs"), file("F", "D", "f.txt")]) }).unwrap();
        let err = f.materializer(true, Some(link)).apply(Scope::Full).unwrap_err();
        assert!(matches!(err, ApplyError::Mark(..)), "{err:?}");
        let unmarked = ino(&f.path(".konedrive-new-D"));

        let socket = sockets.path().join("helper.sock");
        let marks = marking_helper(socket.clone());
        let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
        f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
        assert_eq!(ino(&f.path("docs")), unmarked, "the folder made the first time is the one placed");
        assert_eq!(id_at(&f.path("docs/f.txt")).as_deref(), Some("F"));
        let seen: Vec<Marked> = marks.try_iter().collect();
        assert!(
            seen.iter().any(|m| m.ino == unmarked && m.entries == 0 && m.name != "docs"),
            "docs must be marked while empty, before its real name shows it: {seen:?}"
        );
    }

    /// The same for the holding directory: one left by a cycle whose marking
    /// failed is marked before anything is moved into it.
    #[test]
    fn a_holding_directory_whose_marking_failed_is_marked_before_it_is_used() {
        let f = fixture();
        f.listed(&tree(), true);
        let sockets = tempfile::tempdir().unwrap();
        let refusing = sockets.path().join("refusing.sock");
        let _refused = helper_answering(refusing.clone(), libc::EIO);
        let link = f.handle().block_on(HelperLink::connect(&refusing)).unwrap().0;
        f.store.with(|s| { s.begin_staging(true)?; s.stage(&[file("F", "D", "renamed.txt")]) }).unwrap();
        let err = f.materializer(true, Some(link)).apply(Scope::Changed(vec!["F".into()])).unwrap_err();
        assert!(matches!(err, ApplyError::Mark(..)), "{err:?}");
        let holding = ino(&f.path(".konedrive-holding"));

        let socket = sockets.path().join("helper.sock");
        let marks = marking_helper(socket.clone());
        let link = f.handle().block_on(HelperLink::connect(&socket)).unwrap().0;
        f.materializer(true, Some(link)).apply(Scope::Full).unwrap();
        assert_eq!(id_at(&f.path("docs/renamed.txt")).as_deref(), Some("F"));
        let seen: Vec<Marked> = marks.try_iter().collect();
        assert!(seen.iter().any(|m| m.ino == holding && m.entries == 0), "the holding directory was never marked: {seen:?}");
    }

    /// One `MarkDir` as the helper saw it.
    #[derive(Debug)]
    struct Marked {
        ino: u64,
        /// How many entries the directory held at that moment.
        entries: usize,
        /// Its name at that moment.
        name: String,
    }

    /// Acknowledges everything and reports every `MarkDir` it is sent.
    fn marking_helper(path: PathBuf) -> std::sync::mpsc::Receiver<Marked> {
        helper_answering(path, 0)
    }

    /// Answers every `MarkDir` with `errno` (everything else with success) and
    /// reports each one it is sent.
    fn helper_answering(path: PathBuf, errno: i32) -> std::sync::mpsc::Receiver<Marked> {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
        bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
        sock_listen(&fd, Backlog::new(4).unwrap()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let accepted = accept(fd.as_raw_fd()).unwrap();
            // SAFETY: a descriptor `accept` just returned, owned by nothing else.
            let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let _ = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                let mut answer = 0;
                if let (ToHelper::MarkDir, Some(fd)) = (&message, fd) {
                    let at = format!("/proc/self/fd/{}", fd.as_raw_fd());
                    let entries = std::fs::read_dir(&at).unwrap().count();
                    let name = std::fs::read_link(&at).unwrap().file_name().unwrap().to_string_lossy().into_owned();
                    let ino = File::from(fd).metadata().unwrap().ino();
                    let _ = tx.send(Marked { ino, entries, name });
                    answer = errno;
                }
                if channel.send(&ToDaemon::Ack { errno: answer }, None).is_err() {
                    break;
                }
            }
        });
        rx
    }
}
