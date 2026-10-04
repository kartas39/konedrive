use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use crate::local::entry::{self, Entry};
use crate::local::liveness::Whereabouts;
use konedrive_tree::outbox::{LocalSkip, OutboxRow, OutboxState};
use konedrive_tree::{Kind, Located, Placement, Row, Table, TreeError};

use super::{denied, ExamineError, Expect, gone, Place, Run, Settle};

impl Run<'_, '_> {
    pub(super) fn store<T: Send + 'static>(&self, f: impl FnOnce(&mut konedrive_tree::TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.ex.store.call_blocking(f)
    }

    pub(super) fn base_row(&mut self, id: &str) -> Result<Option<Row>, TreeError> {
        if let Some(row) = self.base.get(id) {
            return Ok(row.clone());
        }
        self.facts(id)?;
        Ok(self.base.get(id).cloned().flatten())
    }

    /// Item `id`'s row, recorded object and place in the base, in one job.
    fn facts(&mut self, id: &str) -> Result<(), TreeError> {
        let asked = id.to_owned();
        let (row, handle, located) = self.store(move |s| Ok((s.get(Table::Items, &asked)?, s.local_handle(&asked)?, s.locate(Table::Items, &asked)?)))?;
        self.base.entry(id.to_owned()).or_insert(row);
        self.recorded.insert(id.to_owned(), (handle, located));
        Ok(())
    }

    /// The local object the base records for item `id`.
    pub(super) fn local_handle(&mut self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        if !self.recorded.contains_key(id) {
            self.facts(id)?;
        }
        Ok(self.recorded.get(id).and_then(|(handle, _)| handle.clone()))
    }

    /// Where the base places item `id`.
    pub(super) fn located(&mut self, id: &str) -> Result<Option<Located>, TreeError> {
        if !self.recorded.contains_key(id) {
            self.facts(id)?;
        }
        Ok(self.recorded.get(id).and_then(|(_, located)| located.clone()))
    }

    /// The live row of a local object with no item id yet.
    pub(super) fn pending_row(&self, e: &Entry) -> Option<&OutboxRow> {
        self.rows.of_object(e).last().copied()
    }

    /// Whether the worker is creating `e`'s object in OneDrive right now.
    pub(super) fn being_created(&self, e: &Entry) -> bool {
        self.rows.of_object(e).iter().any(|row| row.state == OutboxState::Running)
    }

    /// Where item `id` should be: where its live row last saw it, or else
    /// its base name under where its parent should be — so the items in a
    /// folder with a pending move are looked for where the folder is now.
    /// Cached, and derived from the parent's answer, so that a Full scan
    /// costs one lookup per item rather than one walk up the tree.
    pub(super) fn expected(&mut self, id: &str) -> Result<Expect, TreeError> {
        self.expected_at(id, 0)
    }

    fn expected_at(&mut self, id: &str, depth: usize) -> Result<Expect, TreeError> {
        if let Some(expect) = self.expected.get(id) {
            return Ok(expect.clone());
        }
        let last = self.rows.of_item(id).next_back().map(|row| (row.kind.removes(), row.rel.clone()));
        let expect = match last {
            Some((true, _)) => Expect::Nowhere,
            Some((false, rel)) => Expect::At(rel),
            None => match self.base_row(id)? {
                None => Expect::Unknown,
                Some(row) if row.placement != Placement::Placed => Expect::Nowhere,
                Some(row) => match row.parent_id.as_deref() {
                    None => Expect::At(PathBuf::new()),
                    Some(parent) if parent == self.root_id => Expect::At(PathBuf::from(&row.name)),
                    // A cycle or corruption, not a drive.
                    Some(_) if depth > konedrive_fs::MAX_DEPTH => Expect::Nowhere,
                    Some(parent) => match self.expected_at(parent, depth + 1)? {
                        Expect::At(dir) => Expect::At(dir.join(&row.name)),
                        _ => Expect::Nowhere,
                    },
                },
            },
        };
        self.expected.insert(id.to_owned(), expect.clone());
        Ok(expect)
    }

    /// Whether entry `i`, carrying item id `id`, is a name of the leaving
    /// object's own inode — its handle, or, with none kept, the object at
    /// its recorded place — not merely another object with that id.
    pub(super) fn is_leaving_inode(&self, id: &str, i: usize) -> bool {
        let Some(&n) = self.leaving_index.get(id) else { return false };
        match self.leaving_ids.get(id) {
            Some((_, handle)) => self.entries[i].handle.as_ref() == Some(handle),
            None => self.at.get(&self.leaving[n]).is_some_and(|&j| self.entries[j].same_object(&self.entries[i])),
        }
    }

    /// Whether `rel` is at or below an object that is leaving (issue #104).
    pub(super) fn under_leaving(&self, rel: &Path) -> bool {
        self.leaving.iter().enumerate().any(|(n, at)| !self.leaving_elsewhere.contains(&n) && rel.starts_with(at))
    }

    pub(super) fn push(&mut self, e: Entry) -> usize {
        match self.at.get(&e.rel) {
            Some(&i) => {
                self.entries[i] = e;
                i
            }
            None => {
                self.at.insert(e.rel.clone(), self.entries.len());
                self.entries.push(e);
                self.entries.len() - 1
            }
        }
    }

    pub(super) fn mark_unreadable(&mut self, rel: &Path, why: &io::Error) {
        if self.unreadable.insert(rel.to_path_buf()) {
            // One line for the run says how many (`Examiner::examine_reporting`).
            tracing::debug!("{} cannot be read ({why}); it is not examined", rel.display());
            self.out.unreadable.push(rel.to_path_buf());
        }
    }

    /// The one policy for an entry that cannot be opened, stripped or read
    /// (`LO3`). Gone since it was listed, it is skipped (`None`). Refused to
    /// this daemon (`EACCES`, `EPERM`: another user's file, an immutable
    /// one), the trouble is the entry's own: it is passed over
    /// ([`pass_over`](Self::pass_over), `None`), never the batch's failure.
    /// Any other error may be anybody's (no descriptors, no memory, the
    /// disk): the batch fails, as it always did, and is offered again.
    pub(super) fn entry_io<T>(&mut self, e: &Entry, tried: io::Result<T>) -> Result<Option<T>, ExamineError> {
        match tried {
            Ok(value) => Ok(Some(value)),
            Err(err) if gone(&err) => Ok(None),
            Err(err) if denied(&err) => {
                self.pass_over(e, &err);
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// `e` is not examined in this run: noted as unreadable, so that nothing
    /// at its place counts as missing, and asked for again
    /// ([`Examined::passed`]).
    pub(super) fn pass_over(&mut self, e: &Entry, why: &io::Error) {
        self.mark_unreadable(&e.rel, why);
        self.out.passed.name(e.dir_rel(), &e.name);
    }

    /// `name` in `dir`, or `None` when there is nothing there, or nothing
    /// this daemon may read (then noted as unreadable).
    pub(super) fn read_entry(&mut self, dir: &File, dir_rel: &Path, name: &OsStr) -> Result<Option<Entry>, ExamineError> {
        match entry::read(dir, dir_rel, name) {
            Ok(e) => Ok(e),
            Err(e) if denied(&e) => {
                self.mark_unreadable(&dir_rel.join(name), &e);
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Reads `name` in the directory at `dir_rel`, if both are there.
    pub(super) fn read_one(&mut self, dir_rel: &Path, name: &OsStr) -> Result<Option<usize>, ExamineError> {
        if let Some(&i) = self.at.get(&dir_rel.join(name)) {
            return Ok(Some(i));
        }
        let dir = match self.ex.disk.dir(dir_rel) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) if denied(&e) => {
                self.mark_unreadable(dir_rel, &e);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        Ok(self.read_entry(&dir, dir_rel, name)?.map(|e| self.push(e)))
    }

    /// Whether a directory's contents are new to the folder: no id, an id
    /// the base does not have as a folder, or a folder's id on another inode
    /// than the one recorded (a copy that kept its attributes).
    pub(super) fn is_new_dir(&mut self, e: &Entry) -> Result<bool, TreeError> {
        let Some(id) = &e.id else { return Ok(true) };
        match self.base_row(id)? {
            Some(row) if row.kind == Kind::Folder => {
                let recorded = self.local_handle(id)?;
                Ok(recorded.is_some() && e.handle.is_some() && recorded != e.handle)
            }
            _ => Ok(true),
        }
    }

    /// The item id of the directory at `rel` as this batch decided it:
    /// `None` for a directory new to OneDrive (its `mkdir` is pending).
    pub(super) fn dir_id(&self, rel: &Path) -> Option<String> {
        if rel.as_os_str().is_empty() {
            return Some(self.root_id.clone());
        }
        let &i = self.at.get(rel)?;
        let id = self.entries[i].id.as_ref()?;
        (self.chosen.get(id) == Some(&i)).then(|| id.clone())
    }

    /// Where the object `handle` names is: the liveness answer, placed. An
    /// answer is "outside" only when the path is sure and not beneath the
    /// root; "inside" only when the object's own handle stands at that place
    /// beneath the root. Anything else decides nothing.
    pub(super) fn place_of(&self, handle: &FileHandle) -> Place {
        let path = match self.ex.liveness.whereabouts(handle) {
            Ok(Whereabouts::Gone) if self.handles_current => return Place::Gone,
            Ok(Whereabouts::Gone) => return Place::Unknown,
            Ok(Whereabouts::At(path)) => path,
            Err(err) => {
                tracing::debug!("an object's whereabouts cannot be asked yet: {err}");
                return Place::Unknown;
            }
        };
        let Some(root) = &self.root_path else { return Place::Unknown };
        let sure = path.is_absolute()
            && !path.as_os_str().as_bytes().ends_with(b" (deleted)")
            && path.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
        if !sure {
            return Place::Unknown;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return Place::Outside(path);
        };
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return Place::Unknown };
        match self.ex.disk.dir(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()) {
            Some(there) if &there == handle => Place::Inside(rel.to_path_buf()),
            _ => Place::Unknown,
        }
    }

    /// Whether the object `handle` names is proved absent from its place `rel`
    /// beneath the root ([`crate::local::liveness::absent_below`]).
    pub(super) fn absent(&self, rel: &Path, handle: &FileHandle) -> bool {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return false };
        crate::local::liveness::absent_below(self.ex.disk.dir(parent), name, handle)
    }

    /// [`absent`](Self::absent) for `at`, inside the folder at `folder` — which
    /// is now at `went_to` if it left the folder.
    pub(super) fn absent_with(&self, at: &Path, folder: &Path, went_to: Option<&Path>, handle: &FileHandle) -> bool {
        match (went_to, at.strip_prefix(folder)) {
            (Some(went), Ok(inside)) => crate::local::liveness::absent_at(&went.join(inside), handle),
            (Some(_), Err(_)) => false,
            (None, _) => self.absent(at, handle),
        }
    }

    pub(super) fn skip(&mut self, rel: &Path, reason: LocalSkip) {
        self.skipped.insert(rel.to_path_buf(), reason);
    }

    pub(super) fn recheck(&mut self, e: &Entry) {
        self.out.recheck.name(e.dir_rel(), &e.name);
    }

    /// Item `id` is decided without a row: remembered for a folder it is
    /// in, and, when `report`, listed as undecided or unproven.
    pub(super) fn hold_back(&mut self, id: &str, settle: Settle, report: bool) {
        self.decided.insert(id.to_owned());
        self.deferred.insert(id.to_owned(), settle);
        match (settle, report) {
            (Settle::Wait, true) => self.out.undecided.push(id.to_owned()),
            (Settle::Unproven, true) => self.out.unproven.push(id.to_owned()),
            _ => {}
        }
    }

    pub(super) fn recheck_at(&mut self, rel: &Path) {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.out.recheck.name(parent, name);
        }
    }
}
