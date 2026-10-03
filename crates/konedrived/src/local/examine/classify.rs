use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder;
use crate::local::batch::Batch;
use crate::local::entry::{Entry, StateAttr, Type};
use crate::local::names;
use crate::local::snapshot;
use konedrive_graph::drive::item::RESERVED_PREFIX;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Kind, Row, Table};

use super::{daemon_owned, depth, ExamineError, Expect, gone, lossy, MOUNTED_INSIDE, object, OTHER_DEVICE, Place, Run, Settle};

impl Run<'_, '_> {
    pub(super) fn classify(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let mut by_id: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut unnamed: Vec<usize> = Vec::new();
        let mut leaving: Vec<usize> = Vec::new();
        // A leaving object is found by its own file handle wherever it is
        // now — a parent renamed here or in OneDrive took it along (issue
        // #104) — never by its item id alone: the copy placed again, a copy
        // or a hard link carry the id too. With no handle kept (a store from
        // before it was), by its path only.
        // A file with other links is followed by its path only: its hard
        // link carries the same handle, and is the user's name.
        // At its recorded place, an object with its id is it — an editor's
        // save by rename makes a new inode, whose handle is taken anew —
        // unless the item is placed elsewhere: then the copy placed again may
        // stand there by the user's move, and only the handle tells.
        for (id, (n, handle)) in self.leaving_ids.clone() {
            let Some(&i) = self.at.get(&self.leaving[n]) else { continue };
            let Some(there) = self.entries[i].handle.clone().filter(|h| *h != handle) else { continue };
            let rel = self.leaving[n].clone();
            let ours = self.entries[i].id.as_deref() == Some(id.as_str())
                && !self.store({ let (id, rel) = (id.clone(), rel.clone()); move |s| s.placed_elsewhere(&id, &rel) })?;
            if ours {
                self.leaving_ids.insert(id.clone(), (n, there.clone()));
                self.store(move |s| s.leaving_set_handle(&id, &there))?;
            } else {
                self.leaving_elsewhere.insert(n);
            }
        }
        for i in 0..self.entries.len() {
            let Some(id) = self.entries[i].id.clone() else { continue };
            let Some((n, handle)) = self.leaving_ids.get(&id).cloned() else { continue };
            let rel = self.entries[i].rel.clone();
            let linked = self.entries[i].ty == Type::File && self.entries[i].nlink > 1;
            if self.leaving[n] == rel || linked || self.entries[i].handle.as_ref() != Some(&handle) {
                continue;
            }
            self.leaving_elsewhere.remove(&n);
            self.leaving[n] = rel.clone();
            self.store(move |s| s.leaving_set_rel(&id, &rel))?;
        }
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            // 1. The daemon's own names; a user's `.konedrive-*` is listed.
            if daemon_owned(&e.name) {
                continue;
            }
            if e.name.as_bytes().starts_with(RESERVED_PREFIX.as_bytes()) {
                let rel = e.rel.clone();
                self.skip(&rel, "reserved-name");
                continue;
            }
            // 2. Never a OneDrive object — unless its name is ignored anyway
            // (Emacs's `.#name` lock is a symlink).
            if let Some(reason) = e.ty.skip_reason() {
                if !self.ex.ignore.matches(&e.name) {
                    let rel = e.rel.clone();
                    self.skip(&rel, reason);
                }
                continue;
            }
            // 2b. On another device than the folder's (a nested Btrfs
            // subvolume, a mount): never uploaded, nor anything below it —
            // the helper cannot protect what is placed there (F72).
            if e.dev != self.root_dev {
                let rel = e.rel.clone();
                // Inside a folder that is leaving, it keeps the folder on
                // disk, and says so (issue #104).
                let reason = if self.under_leaving(&rel) { MOUNTED_INSIDE } else { OTHER_DEVICE };
                self.skip(&rel, reason);
                continue;
            }
            match &e.id {
                // An object that is leaving, or inside one: never the item
                // where it is placed now, never a stranger (issue #104).
                Some(_) if self.under_leaving(&e.rel) => leaving.push(i),
                // Another name of a leaving file (a hard link the user made):
                // never the item's name in OneDrive, listed as one.
                Some(id) if e.ty == Type::File && e.nlink > 1 && self.is_leaving_inode(id, i) => {
                    let rel = e.rel.clone();
                    if !self.ex.ignore.matches(&e.name) {
                        self.skip(&rel, "hard-link");
                    }
                }
                Some(id) => by_id.entry(id.clone()).or_default().push(i),
                None => unnamed.push(i),
            }
        }
        // An object inside a leaving folder whose id the base does not know
        // is a stranger there as anywhere: its content goes up as new.
        let mut ours = Vec::new();
        for i in leaving {
            let id = self.entries[i].id.clone().expect("only entries with an id");
            if self.base_row(&id)?.is_some() {
                // A placed item the user moved in is the user's move, carried
                // out as any other; what was in it, or is placed nowhere,
                // only uploads its content (issue #104).
                let placed = self.located(&id)?.is_some_and(|l| l.placed);
                if placed && !self.store({ let id = id.clone(); move |s| s.leaving_had(&id) })? {
                    by_id.entry(id).or_default().push(i);
                } else {
                    ours.push(i);
                }
            } else if self.store({ let id = id.clone(); move |s| s.leaving_had(&id) })? {
                // Was inside it when it began to leave, and is gone from the
                // base since: removed in OneDrive, never uploaded as new
                // (issue #104, decision 2). The reconcile removes it.
                self.consumed.insert(i);
            } else {
                by_id.entry(id).or_default().push(i);
            }
        }
        let leaving = ours;
        // 6. Who is who, before anything is decided by place: a directory's
        // id says what its entries' parent is.
        let mut found: Vec<(String, usize)> = Vec::new();
        for (id, entries) in &by_id {
            if let Some(chosen) = self.resolve(id, entries)? {
                found.push((id.clone(), chosen));
            }
        }
        found.sort_by_key(|(_, i)| depth(&self.entries[*i].rel));
        for (id, i) in found {
            self.found(&id, i, batch)?;
        }
        // What is leaving uploads its content into its item, nothing more.
        for i in leaving {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            let Some(id) = e.id.clone() else { continue };
            if let Some(base) = self.base_row(&id)? {
                self.found_leaving(&id, &base, &e, batch)?;
            }
        }
        // 7. What is missing from where it was.
        self.missing()?;
        // 3–5. What has no id (or no longer has one).
        unnamed.extend(std::mem::take(&mut self.fresh));
        unnamed.sort_by_key(|&i| depth(&self.entries[i].rel));
        unnamed.dedup();
        for i in unnamed {
            if !self.consumed.contains(&i) {
                self.unnamed(i)?;
            }
        }
        self.tidy_skipped(batch.full)?;
        Ok(())
    }

    /// Decides which of the entries carrying `id` is the item, and what the
    /// others are. Returns the item's entry, if one is.
    fn resolve(&mut self, id: &str, entries: &[usize]) -> Result<Option<usize>, ExamineError> {
        let Some(base) = self.base_row(id)? else {
            // Unknown to the base.
            let placing = self.store({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?.is_some();
            for &i in entries {
                let e = self.entries[i].clone();
                if placing {
                    // A reconcile is placing it right now; its swap follows.
                    self.recheck(&e);
                } else if self.pending_row(&e).is_some() {
                    // A create or mkdir between its two commit steps (§5): its
                    // replay adopts it.
                } else {
                    self.stranger(i)?;
                }
            }
            return Ok(None);
        };
        let want = if base.kind == Kind::Folder { Type::Dir } else { Type::File };
        let mut same: Vec<usize> = Vec::new();
        for &i in entries {
            if self.entries[i].ty == want {
                same.push(i);
            } else {
                // Its own id with the other kind: not the item.
                self.stranger(i)?;
            }
        }
        if same.is_empty() {
            return Ok(None);
        }
        // Group by object: one inode may have several names (hard links).
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for &i in &same {
            match groups.iter_mut().find(|g| self.entries[g[0]].same_object(&self.entries[i])) {
                Some(group) => group.push(i),
                None => groups.push(vec![i]),
            }
        }
        for group in &mut groups {
            group.sort_by(|&a, &b| self.entries[a].rel.cmp(&self.entries[b].rel));
        }
        let recorded = self.local_handle(id)?;
        let expect = self.expected(id)?;
        let expected_rel = match &expect {
            Expect::At(rel) => Some(rel.clone()),
            _ => None,
        };
        let base_rel = self.located(id)?.filter(|l| l.placed).map(|l| l.rel);
        let is_at = |run: &Self, g: &[usize], rel: &Option<PathBuf>| rel.as_ref().is_some_and(|r| g.iter().any(|&i| &run.entries[i].rel == r));
        let original = recorded.as_ref().and_then(|h| groups.iter().position(|g| self.entries[g[0]].handle.as_ref() == Some(h)));

        // Rename to a backup, write new (vim's `file~`): the recorded inode
        // sits under an ignored name beside where the item is expected, and
        // another file stands there.
        if base.kind == Kind::File {
            if let (Some(o), Some(rel)) = (original, &expected_rel) {
                let group = groups[o].clone();
                let beside = group.iter().all(|&i| self.entries[i].dir_rel() == rel.parent().unwrap_or(Path::new("")))
                    && group.iter().any(|&i| self.ex.ignore.matches(&self.entries[i].name));
                // A new file, or one that copied the old one's attributes —
                // not one the worker is creating right now.
                let newcomer = self.at.get(rel).copied().filter(|&s| {
                    let e = &self.entries[s];
                    !group.contains(&s) && e.ty == Type::File && (e.id.is_none() || e.id.as_deref() == Some(id)) && !self.being_created(e)
                });
                if let (true, Some(s)) = (beside && !is_at(self, &group, &expected_rel), newcomer) {
                    self.backup(id, &base, &group, s)?;
                    return Ok(None);
                }
            }
        }

        let at_place = groups.iter().position(|g| is_at(self, g, &expected_rel)).or_else(|| groups.iter().position(|g| is_at(self, g, &base_rel)));
        let pick = match (original, at_place, recorded) {
            (Some(o), _, _) => o,
            // The one where the item is: an editor's new inode that copied
            // its attributes.
            (None, Some(p), _) => p,
            (None, None, None) => {
                tracing::warn!("{id} is on {} inodes, none of them where it was: the first by path is taken", groups.len());
                0
            }
            // None of these is the inode the base records, and none stands
            // where the item is: where is the recorded one (§3.4)?
            (None, None, Some(handle)) => match self.place_of(&handle) {
                Place::Gone => {
                    tracing::warn!("{id} is on {} inodes, none of them where it was: the first by path is taken", groups.len());
                    0
                }
                Place::Outside(to) => {
                    // It left the folder: these are copies it left behind.
                    for &i in &same {
                        self.stranger(i)?;
                    }
                    if let Some(rel) = &expected_rel {
                        self.removal(OutboxKind::MoveOut, id, &base, rel, Some(object(handle)), Some(to.as_path()))?;
                    }
                    self.decided.insert(id.to_owned());
                    return Ok(None);
                }
                Place::Inside(now) => {
                    self.recheck_at(&now);
                    self.hold_back(id, Settle::Wait, false);
                    return Ok(None);
                }
                Place::Unknown => {
                    for &i in &same {
                        let e = self.entries[i].clone();
                        self.recheck(&e);
                    }
                    self.hold_back(id, Settle::Wait, true);
                    return Ok(None);
                }
            },
        };
        let group = groups[pick].clone();
        let chosen = group
            .iter()
            .copied()
            .find(|&i| Some(&self.entries[i].rel) == expected_rel.as_ref())
            .unwrap_or(group[0]);
        self.chosen.insert(id.to_owned(), chosen);
        self.consumed.insert(chosen);
        // Other names of the same object: a hard link OneDrive cannot hold.
        for &i in group.iter().filter(|&&i| i != chosen) {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if !self.ex.ignore.matches(&e.name) {
                self.skip(&e.rel, "hard-link");
            }
        }
        // Other objects with its id: copies that kept its attributes.
        for (n, other) in groups.iter().enumerate() {
            if n != pick {
                for &i in other {
                    self.stranger(i)?;
                }
            }
        }
        Ok(Some(chosen))
    }

    /// An entry with an item id that is not the item's (a copy, a file from
    /// another folder or account, the wrong kind): downloaded, it is the
    /// user's own file, stripped and uploaded as new; a directory likewise,
    /// with its contents. A placeholder cannot be read here and is listed; so
    /// is a file with other links, whose other names stripping would change
    /// too.
    fn stranger(&mut self, i: usize) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let dir = match self.ex.disk.dir(e.dir_rel()) {
            Ok(dir) => dir,
            Err(err) if gone(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let listed = |run: &mut Self, reason: &str| {
            if !run.ex.ignore.matches(&e.name) {
                run.skip(&e.rel, reason);
            }
            run.consumed.insert(i);
        };
        match e.ty {
            Type::Dir => {
                placeholder::strip_konedrive_xattrs(&self.ex.disk.open_subdir(&dir, &e.name)?)?;
                if !self.whole.contains(&e.rel) {
                    self.out.recheck.tree(&e.rel);
                }
            }
            Type::File if e.hydrated() && e.nlink > 1 => {
                listed(self, "hard-link");
                return Ok(());
            }
            Type::File if e.hydrated() => placeholder::strip_konedrive_xattrs(&self.ex.disk.open_file(&dir, &e.name)?)?,
            _ => {
                listed(self, "not-downloaded");
                return Ok(());
            }
        }
        self.out.stripped.push(e.rel.clone());
        self.entries[i].id = None;
        self.entries[i].state = StateAttr::Absent;
        self.fresh.push(i);
        Ok(())
    }

    /// Save-by-rename with a backup: item `id`'s inode (`group`) now sits
    /// under an ignored name, and `s` stands where the item is. `s` is the
    /// item's new content; the backup stays the user's, stripped if it was
    /// downloaded (a placeholder stays managed and fills on open).
    fn backup(&mut self, id: &str, base: &Row, group: &[usize], s: usize) -> Result<(), ExamineError> {
        for &i in group {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if e.hydrated() {
                let dir = self.ex.disk.dir(e.dir_rel())?;
                placeholder::strip_konedrive_xattrs(&self.ex.disk.open_file(&dir, &e.name)?)?;
                self.out.stripped.push(e.rel);
            }
        }
        self.consumed.insert(s);
        self.chosen.insert(id.to_owned(), s);
        if self.entries[s].id.is_some() {
            // A new file that copied the old one's attributes (vim does):
            // its stamp and cTag are the old version's, not its own.
            self.entries[s].state = StateAttr::Absent;
        }
        self.save_by_rename(id, base, s)
    }

    /// Item `id`'s content is now the file `s` at its place: an `update`
    /// from the new inode, which takes over the item at commit. A pending
    /// create of that file goes (never one being sent: callers exclude it).
    pub(super) fn save_by_rename(&mut self, id: &str, base: &Row, s: usize) -> Result<(), ExamineError> {
        let e = self.entries[s].clone();
        if let Some(row) = self.pending_row(&e).filter(|row| row.state != OutboxState::Running) {
            self.ops.push(OutboxOp::Remove(row.seq));
        }
        let (state, reason, next_try) = self.probe_writer(&e)?;
        let mut d = self.detection(OutboxKind::Update, id, base, &e, None);
        (d.state, d.reason, d.next_try) = (state, reason, next_try);
        self.detections.push(d);
        Ok(())
    }

    pub(super) fn detection(&self, kind: OutboxKind, id: &str, base: &Row, e: &Entry, local_ctag: Option<&str>) -> Detection {
        // The version the local content derives from: the file's own cTag
        // when it names another than the base's (a download not yet
        // replaced); the eTag guards only the base's own version.
        let same_version = local_ctag.is_none_or(|c| Some(c) == base.ctag.as_deref());
        Detection {
            kind,
            item_id: Some(id.to_owned()),
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: Some(Base {
                etag: if same_version { base.etag.clone() } else { None },
                ctag: if same_version { base.ctag.clone() } else { local_ctag.map(str::to_owned) },
                parent: base.parent_id.clone(),
                name: Some(base.name.clone()),
            }),
            target_parent: self.dir_id(e.dir_rel()),
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: (e.ty == Type::File).then_some(e.size),
        }
    }

    /// Rules 3–5: an entry without an item id.
    fn unnamed(&mut self, i: usize) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let pending = self.pending_row(&e).cloned();
        let target_parent = self.dir_id(e.dir_rel());
        // 3. Ignored — its name, or a directory of the user's own above it
        // (the outbox on the bus): stays local, and a create it had goes — unless the
        // worker is creating it right now: then it is an item already, which
        // syncs whatever its name, and the rename waits behind.
        if self.ex.ignore.matches(&e.name) || self.in_ignored_dir(e.dir_rel()) {
            let being_created = self.being_created(&e);
            if being_created {
                self.detections.push(Detection {
                    kind: OutboxKind::Move,
                    item_id: None,
                    inode: Some(e.inode()),
                    rel: e.rel.clone(),
                    base: None,
                    target_parent,
                    target_name: Some(lossy(&e.name)),
                    same_content: false,
                    state: OutboxState::Ready,
                    reason: None,
                    next_try: None,
                    size: None,
                });
            } else if let Some(row) = pending.filter(|r| matches!(r.kind, OutboxKind::Create | OutboxKind::Mkdir)) {
                self.ops.push(OutboxOp::Remove(row.seq));
            }
            // A directory newly ignored takes the new things waiting inside it
            // along: their folder is never made in OneDrive (the outbox on the bus).
            // One being made keeps them: it is an item already.
            if e.ty == Type::Dir && !being_created {
                let inside: Vec<i64> =
                    self.rows.under(&e.rel).into_iter().filter(|r| r.item_id.is_none() && r.state != OutboxState::Running).map(|r| r.seq).collect();
                self.ops.extend(inside.into_iter().map(OutboxOp::Remove));
            }
            return Ok(());
        }
        let is_dir = e.ty == Type::Dir;
        // A file over a name whose delete is still pending: save-by-rename
        // across batches (§3.5).
        if !is_dir && pending.is_none() {
            let delete =
                self.rows.at(&e.rel).find(|r| r.kind == OutboxKind::Delete && r.state != OutboxState::Running && r.item_id.is_some()).cloned();
            if let Some(row) = delete {
                let id = row.item_id.clone().expect("filtered above");
                if let Some(base) = self.base_row(&id)?.filter(|b| b.kind == Kind::File) {
                    self.consumed.insert(i);
                    return self.save_by_rename(&id, &base, i);
                }
            }
        }
        let mut d = Detection {
            kind: if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create },
            item_id: None,
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: None,
            target_parent,
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: (!is_dir).then_some(e.size),
        };
        if let Some(row) = &pending {
            if is_dir && row.rel != e.rel {
                self.ops.push(OutboxOp::Rebase { from: row.rel.clone(), to: e.rel.clone() });
            }
            let now = snapshot(e.size, e.mtime.0, e.mtime.1);
            if !is_dir && row.state == OutboxState::Running && row.snapshot.as_deref() == Some(now.as_str()) {
                // Being uploaded as it is now: only where it is matters.
                d.kind = OutboxKind::Move;
            }
        }
        let refused = names::refused(&e.name).or((!is_dir && e.size > names::MAX_FILE_SIZE).then_some(names::Refused::TooLarge));
        if let Some(refused) = refused {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.as_str().into());
        } else if !is_dir {
            (d.state, d.reason, d.next_try) = self.probe_writer(&e)?;
        }
        self.detections.push(d);
        Ok(())
    }
}
