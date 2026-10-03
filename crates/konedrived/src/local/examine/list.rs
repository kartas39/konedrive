use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use crate::local::batch::{Batch, DirScope};
use crate::local::entry::Type;
use konedrive_tree::TreeError;

use super::{daemon_owned, denied, depth, ExamineError, Expect, gone, Run};

impl Run<'_, '_> {
    /// Reads every place the batch names, shallowest first; a new
    /// directory's contents with it.
    pub(super) fn list(&mut self, batch: &Batch) -> Result<(), ExamineError> {
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
            if let Some(rel) = self.expected_of_handle(handle)? {
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    seed(&mut seeds, parent.to_path_buf(), DirScope::Names([name.to_owned()].into()), false);
                }
            }
        }
        let mut seeds: Vec<_> = seeds.into_iter().collect();
        seeds.sort_by_key(|(rel, _)| depth(rel));
        let mut queue: VecDeque<(PathBuf, DirScope, bool)> = seeds.into_iter().map(|(rel, (scope, recurse))| (rel, scope, recurse)).collect();
        let mut recursed: HashSet<PathBuf> = HashSet::new();
        while let Some((rel, scope, recurse)) = queue.pop_front() {
            if recurse && !recursed.insert(rel.clone()) {
                continue;
            }
            if !recurse && self.whole.contains(&rel) {
                continue;
            }
            self.list_dir(&rel, scope, recurse, &mut queue)?;
        }
        Ok(())
    }

    fn list_dir(&mut self, rel: &Path, scope: DirScope, recurse: bool, queue: &mut VecDeque<(PathBuf, DirScope, bool)>) -> Result<(), ExamineError> {
        if depth(rel) >= konedrive_fs::MAX_DEPTH {
            tracing::warn!("{} is deeper than {} levels; not examined", rel.display(), konedrive_fs::MAX_DEPTH);
            return Ok(());
        }
        if rel.components().any(|c| daemon_owned(c.as_os_str())) {
            return Ok(());
        }
        let dir = match self.ex.disk.dir(rel) {
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
                self.mark_unreadable(rel, &e);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.read_one(parent, name)?;
        }
        // On another device than the folder's: listed once as not uploaded
        // (its own entry, just read), and nothing inside it is examined.
        if nix::sys::stat::fstat(&dir).map_err(io::Error::from)?.st_dev as u64 != self.root_dev {
            return Ok(());
        }
        let mut whole = matches!(scope, DirScope::Whole);
        let mut read = Vec::new();
        if let DirScope::Names(names) = &scope {
            for name in names {
                match self.read_entry(&dir, rel, name)? {
                    Some(e) => read.push(e),
                    // Not there (a delete, a rename's old side, an O_TMPFILE
                    // pseudo-name, a merged event): the whole directory (§17).
                    None if !self.unreadable.contains(&rel.join(name)) => {
                        whole = true;
                        break;
                    }
                    None => {}
                }
            }
            if !whole {
                self.named.entry(rel.to_path_buf()).or_default().extend(names.iter().cloned());
            }
        }
        if whole {
            read.clear();
            let names = match self.ex.disk.list(&dir) {
                Ok(names) => names,
                Err(e) if denied(&e) => {
                    self.mark_unreadable(rel, &e);
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            for name in names {
                if let Some(e) = self.read_entry(&dir, rel, &name)? {
                    read.push(e);
                }
            }
            self.whole.insert(rel.to_path_buf());
            self.named.remove(rel);
            if let Some(progress) = self.progress {
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
            let ignored = e.id.is_none() && self.ex.ignore.matches(&e.name);
            if e.ty == Type::Dir && e.dev == self.root_dev && !daemon_owned(&e.name) && !ignored && (recurse || self.is_new_dir(&e)?) {
                queue.push_back((e.rel.clone(), DirScope::Whole, true));
            }
            self.push(e);
        }
        Ok(())
    }

    /// Where the item or pending row an event's object handle names is
    /// expected: the place to look.
    fn expected_of_handle(&mut self, handle: &FileHandle) -> Result<Option<PathBuf>, TreeError> {
        if let Some(item) = self.store({ let handle = handle.to_owned(); move |s| s.item_by_handle(&handle) })? {
            if let Expect::At(rel) = self.expected(&item.id)? {
                return Ok(Some(rel));
            }
        }
        let handle = handle.clone();
        Ok(self.store(move |s| s.outbox_by_handle(&handle))?.map(|row| row.rel))
    }

    /// An item seen away from where it is expected is looked for there too:
    /// a copy that kept its attributes must meet its original.
    pub(super) fn probe_expected(&mut self) -> Result<(), ExamineError> {
        let ids: BTreeSet<String> = self.entries.iter().filter_map(|e| e.id.clone()).collect();
        for id in ids {
            let Expect::At(rel) = self.expected(&id)? else { continue };
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { continue };
            if self.at.contains_key(&rel) || self.whole.contains(parent) {
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
