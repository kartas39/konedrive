//! What one examination read of the disk: every place its batch names, by name
//! (`lstat`, `lgetxattr`), never an open. Built once ([`Listing::read`]) and read-only
//! from then on: what the run decides about an entry is kept beside it
//! ([`Decisions`](super::decisions::Decisions)), never written into it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use konedrive_tree::Kind;

use super::facts::{Expect, Facts};
use super::{daemon_owned, denied, depth, gone, ExamineError, ScanProgress};
use crate::folder::disk::Disk;
use crate::local::batch::{Batch, DirScope};
use crate::local::entry::{self, Entry, Type};
use crate::local::ignore::IgnoreList;

/// An entry of a [`Listing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct EntryIx(usize);

#[cfg(test)]
impl EntryIx {
    pub(super) fn nth(n: usize) -> Self {
        EntryIx(n)
    }
}

/// What a listing is read with.
pub(super) struct Reader<'e> {
    pub(super) disk: &'e Disk,
    pub(super) ignore: &'e IgnoreList,
    /// The folder's device: nothing on another one is looked into (a nested
    /// Btrfs subvolume, a mount).
    pub(super) root_dev: u64,
    /// Told after each directory a Full local scan lists.
    pub(super) progress: Option<&'e dyn ScanProgress>,
}

#[derive(Default)]
pub(super) struct Listing {
    entries: Vec<Entry>,
    at: HashMap<PathBuf, EntryIx>,
    /// The whole folder was asked for.
    full: bool,
    /// Directories read whole.
    whole: BTreeSet<PathBuf>,
    /// Directories of which only these names were read.
    named: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// Places this daemon may not read, in the order met.
    unread: Vec<PathBuf>,
    unread_set: HashSet<PathBuf>,
}

impl std::ops::Index<EntryIx> for Listing {
    type Output = Entry;

    fn index(&self, ix: EntryIx) -> &Entry {
        &self.entries[ix.0]
    }
}

#[cfg(test)]
impl Listing {
    /// A listing of these entries, for the rules that need no disk.
    pub(super) fn of(entries: Vec<Entry>) -> Listing {
        let at = entries.iter().enumerate().map(|(n, e)| (e.rel.clone(), EntryIx(n))).collect();
        Listing { entries, at, full: true, ..Listing::default() }
    }
}

impl Listing {
    /// Reads every place `batch` names, shallowest first, a new directory's
    /// contents with it; and, for an item seen away from where it is
    /// expected, that place too: a copy that kept its attributes must meet
    /// its original.
    pub(super) fn read(with: &Reader<'_>, facts: &mut Facts<'_>, batch: &Batch) -> Result<Listing, ExamineError> {
        let mut reading = Reading { with, facts, listing: Listing { full: batch.full, ..Listing::default() }, seen: (0, 0) };
        reading.list(batch)?;
        reading.probe_expected()?;
        Ok(reading.listing)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (EntryIx, &Entry)> + '_ {
        self.entries.iter().enumerate().map(|(i, e)| (EntryIx(i), e))
    }

    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry read at `rel`.
    pub(super) fn at(&self, rel: &Path) -> Option<EntryIx> {
        self.at.get(rel).copied()
    }

    /// Whether the directory at `rel` was read whole.
    pub(super) fn whole(&self, rel: &Path) -> bool {
        self.whole.contains(rel)
    }

    /// The directories read: whole (`None`), or by these names.
    pub(super) fn places(&self) -> Vec<(PathBuf, Option<BTreeSet<OsString>>)> {
        let mut places: Vec<(PathBuf, Option<BTreeSet<OsString>>)> = self.whole.iter().map(|d| (d.clone(), None)).collect();
        places.extend(self.named.iter().map(|(d, n)| (d.clone(), Some(n.clone()))));
        places
    }

    /// The places that could not be read, in the order met.
    pub(super) fn unread(&self) -> &[PathBuf] {
        &self.unread
    }

    /// Whether `rel`, or a directory above it, could not be read.
    pub(super) fn closed(&self, rel: &Path) -> bool {
        !self.unread_set.is_empty() && rel.ancestors().any(|above| self.unread_set.contains(above))
    }

    /// Whether the place `rel` was looked at: it was asked for (the whole
    /// folder, its directory whole, or its name), and neither it nor a
    /// directory above it was closed to this daemon. What stands, or stood,
    /// at a place not looked at is not known: nothing there counts as
    /// missing, and no line of the skipped list is taken off for it. (What
    /// the run gave up on later, while acting, is
    /// [`Decisions::gave_up`](super::decisions::Decisions::gave_up).)
    pub(super) fn examined(&self, rel: &Path) -> bool {
        if self.closed(rel) {
            return false;
        }
        let dir = rel.parent().unwrap_or(Path::new(""));
        self.full || self.whole.contains(dir) || self.named.get(dir).is_some_and(|names| rel.file_name().is_some_and(|n| names.contains(n)))
    }
}

struct Reading<'r, 'e, 'f> {
    with: &'r Reader<'e>,
    facts: &'r mut Facts<'f>,
    listing: Listing,
    /// Directories and other entries read in whole listings so far.
    seen: (u64, u64),
}

type Queue = VecDeque<(PathBuf, DirScope, bool)>;

impl Reading<'_, '_, '_> {
    fn list(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let mut seeds: BTreeMap<PathBuf, (DirScope, bool)> = BTreeMap::new();
        fn seed(seeds: &mut BTreeMap<PathBuf, (DirScope, bool)>, rel: PathBuf, scope: DirScope, recurse: bool) {
            let slot = seeds.entry(rel).or_insert((DirScope::Names(BTreeSet::new()), false));
            slot.1 |= recurse;
            slot.0 = match (std::mem::replace(&mut slot.0, DirScope::Whole), scope) {
                (DirScope::Names(mut a), DirScope::Names(b)) => {
                    a.extend(b);
                    DirScope::Names(a)
                }
                _ => DirScope::Whole,
            };
        }
        if batch.full {
            seed(&mut seeds, PathBuf::new(), DirScope::Whole, true);
        }
        for (dir, scope) in &batch.dirs {
            seed(&mut seeds, dir.clone(), scope.clone(), false);
        }
        for dir in &batch.trees {
            seed(&mut seeds, dir.clone(), DirScope::Whole, true);
        }
        for handle in &batch.objects {
            if let Some(rel) = self.facts.expected_of_handle(handle)? {
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    seed(&mut seeds, parent.to_path_buf(), DirScope::Names([name.to_owned()].into()), false);
                }
            }
        }
        let mut seeds: Vec<_> = seeds.into_iter().collect();
        seeds.sort_by_key(|(rel, _)| depth(rel));
        let mut queue: Queue = seeds.into_iter().map(|(rel, (scope, recurse))| (rel, scope, recurse)).collect();
        let mut recursed: HashSet<PathBuf> = HashSet::new();
        while let Some((rel, scope, recurse)) = queue.pop_front() {
            if recurse && !recursed.insert(rel.clone()) {
                continue;
            }
            if !recurse && self.listing.whole.contains(&rel) {
                continue;
            }
            self.list_dir(&rel, scope, recurse, &mut queue)?;
        }
        Ok(())
    }

    fn list_dir(&mut self, rel: &Path, scope: DirScope, recurse: bool, queue: &mut Queue) -> Result<(), ExamineError> {
        if depth(rel) >= konedrive_fs::MAX_DEPTH {
            tracing::warn!("{} is deeper than {} levels; not examined", rel.display(), konedrive_fs::MAX_DEPTH);
            return Ok(());
        }
        if rel.components().any(|c| daemon_owned(c.as_os_str())) {
            return Ok(());
        }
        let dir = match self.with.disk.dir(rel) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => {
                // Gone since the event: what matters is that it is missing
                // from its parent.
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    queue.push_back((parent.to_path_buf(), DirScope::Names([name.to_owned()].into()), false));
                }
                return Ok(());
            }
            Err(e) if denied(&e) => {
                self.unread(rel, &e);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.read_one(parent, name)?;
        }
        // On another device than the folder's: listed once as not uploaded
        // (its own entry, just read), and nothing inside it is examined.
        if nix::sys::stat::fstat(&dir).map_err(io::Error::from)?.st_dev as u64 != self.with.root_dev {
            return Ok(());
        }
        let mut whole = matches!(scope, DirScope::Whole);
        let mut read = Vec::new();
        if let DirScope::Names(names) = &scope {
            for name in names {
                match self.read_entry(&dir, rel, name)? {
                    Some(e) => read.push(e),
                    // Not there (a delete, a rename's old side, an O_TMPFILE
                    // pseudo-name, a merged event): the whole directory.
                    None if !self.listing.unread_set.contains(&rel.join(name)) => {
                        whole = true;
                        break;
                    }
                    None => {}
                }
            }
            if !whole {
                self.listing.named.entry(rel.to_path_buf()).or_default().extend(names.iter().cloned());
            }
        }
        if whole {
            read.clear();
            let names = match self.with.disk.list(&dir) {
                Ok(names) => names,
                Err(e) if denied(&e) => {
                    self.unread(rel, &e);
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            for name in names {
                if let Some(e) = self.read_entry(&dir, rel, &name)? {
                    read.push(e);
                }
            }
            self.listing.whole.insert(rel.to_path_buf());
            self.listing.named.remove(rel);
            if let Some(progress) = self.with.progress {
                let directories = read.iter().filter(|e| e.ty == Type::Dir).count() as u64;
                self.seen.0 += directories;
                self.seen.1 += read.len() as u64 - directories;
                progress.seen(self.seen.0, self.seen.1);
            }
        }
        for e in read {
            // A directory of the user's own whose name is ignored stays local
            // with everything in it (the outbox on the bus): nothing below it is looked
            // at, so nothing below it waits for a folder never made in OneDrive.
            let ignored = e.id.is_none() && self.with.ignore.matches(&e.name);
            if e.ty == Type::Dir && e.dev == self.with.root_dev && !daemon_owned(&e.name) && !ignored && (recurse || self.is_new_dir(&e)?) {
                queue.push_back((e.rel.clone(), DirScope::Whole, true));
            }
            self.push(e);
        }
        Ok(())
    }

    /// Whether a directory's contents are new to the folder: no id, an id
    /// the base does not have as a folder, or a folder's id on another inode
    /// than the one recorded (a copy that kept its attributes).
    fn is_new_dir(&mut self, e: &Entry) -> Result<bool, ExamineError> {
        let Some(id) = &e.id else { return Ok(true) };
        match self.facts.row(id)? {
            Some(row) if row.kind == Kind::Folder => {
                let recorded = self.facts.recorded(id)?;
                Ok(recorded.is_some() && e.handle.is_some() && recorded != e.handle)
            }
            _ => Ok(true),
        }
    }

    fn push(&mut self, e: Entry) -> EntryIx {
        match self.listing.at.get(&e.rel) {
            Some(&ix) => {
                self.listing.entries[ix.0] = e;
                ix
            }
            None => {
                let ix = EntryIx(self.listing.entries.len());
                self.listing.at.insert(e.rel.clone(), ix);
                self.listing.entries.push(e);
                ix
            }
        }
    }

    fn unread(&mut self, rel: &Path, why: &io::Error) {
        if self.listing.unread_set.insert(rel.to_path_buf()) {
            // One line for the run says how many (`Examiner::examine_reporting`).
            tracing::debug!("{} cannot be read ({why}); it is not examined", rel.display());
            self.listing.unread.push(rel.to_path_buf());
        }
    }

    /// `name` in `dir`, or `None` when there is nothing there, or nothing
    /// this daemon may read (then noted as unread).
    fn read_entry(&mut self, dir: &File, dir_rel: &Path, name: &OsStr) -> Result<Option<Entry>, ExamineError> {
        match entry::read(dir, dir_rel, name) {
            Ok(e) => Ok(e),
            Err(e) if denied(&e) => {
                self.unread(&dir_rel.join(name), &e);
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Reads `name` in the directory at `dir_rel`, if both are there.
    fn read_one(&mut self, dir_rel: &Path, name: &OsStr) -> Result<(), ExamineError> {
        if self.listing.at.contains_key(&dir_rel.join(name)) {
            return Ok(());
        }
        let dir = match self.with.disk.dir(dir_rel) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => return Ok(()),
            Err(e) if denied(&e) => {
                self.unread(dir_rel, &e);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(e) = self.read_entry(&dir, dir_rel, name)? {
            self.push(e);
        }
        Ok(())
    }

    fn probe_expected(&mut self) -> Result<(), ExamineError> {
        let ids: BTreeSet<String> = self.listing.entries.iter().filter_map(|e| e.id.clone()).collect();
        for id in ids {
            let Expect::At(rel) = self.facts.expected(&id)? else { continue };
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { continue };
            if self.listing.at.contains_key(&rel) || self.listing.whole.contains(parent) {
                continue;
            }
            self.read_one(parent, name)?;
            // The directory's own entry, for its id.
            if let (Some(above), Some(own)) = (parent.parent(), parent.file_name()) {
                self.read_one(above, own)?;
            }
        }
        Ok(())
    }
}
