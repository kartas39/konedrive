use std::collections::{BTreeSet, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::local::entry::Type;
use konedrive_tree::outbox::{is_under, place_name, Detection, Inode, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use konedrive_tree::{Kind, Placement, Row, Table};

use super::{base_of, ExamineError, Expect, object, Objects, Place, Run, Settle};

impl Run<'_, '_> {
    /// Rule 7: base items (and pending rows) expected in the examined
    /// places and not found anywhere in the batch.
    pub(super) fn missing(&mut self) -> Result<(), ExamineError> {
        let objects = Objects::of(&self.entries);
        let mut places: Vec<(PathBuf, Option<BTreeSet<OsString>>)> = self.whole.iter().map(|d| (d.clone(), None)).collect();
        places.extend(self.named.iter().map(|(d, n)| (d.clone(), Some(n.clone()))));
        for (dir, names) in places {
            let in_scope = |run: &Self, rel: &Path| {
                !run.unreadable.contains(rel) && names.as_ref().is_none_or(|n| rel.file_name().is_some_and(|f| n.contains(f)))
            };
            let mut items: Vec<(String, PathBuf)> = Vec::new();
            if let Some(parent) = self.dir_id(&dir) {
                for child in self.store({ let parent = parent.to_owned(); move |s| s.children(Table::Items, &parent) })? {
                    if child.placement != Placement::Placed {
                        continue;
                    }
                    self.base.entry(child.id.clone()).or_insert_with(|| Some(child.clone()));
                    if let Expect::At(rel) = self.expected(&child.id)? {
                        if rel.parent() == Some(dir.as_path()) && in_scope(self, &rel) {
                            items.push((child.id.clone(), rel));
                        }
                    }
                }
            }
            let mut pending: Vec<OutboxRow> = Vec::new();
            let listed: HashSet<String> = items.iter().map(|(id, _)| id.clone()).collect();
            for row in self.rows.in_dir(&dir) {
                if row.kind.removes() || !in_scope(self, &row.rel) {
                    continue;
                }
                match &row.item_id {
                    Some(id) => {
                        if !listed.contains(id) && self.rows.of_item(id).next_back().map(|r| r.seq) == Some(row.seq) {
                            items.push((id.clone(), row.rel.clone()));
                        }
                    }
                    None => pending.push(row.clone()),
                }
            }
            for (id, rel) in items {
                if !self.chosen.contains_key(&id) && !self.decided.contains(&id) {
                    self.missing_item(&id, &rel)?;
                }
            }
            for row in pending {
                let seen = row.inode.as_ref().is_some_and(|inode| objects.seen(inode));
                if !seen {
                    self.missing_pending(&row)?;
                }
            }
        }
        Ok(())
    }

    /// A pending create's or mkdir's object is gone, with what waited
    /// inside a new directory.
    fn missing_pending(&mut self, row: &OutboxRow) -> Result<(), ExamineError> {
        self.gone_pending(row);
        if row.kind != OutboxKind::Mkdir {
            return Ok(());
        }
        let inside: Vec<OutboxRow> = self.rows.under(&row.rel).into_iter().cloned().collect();
        for r in inside {
            match &r.item_id {
                None => self.gone_pending(&r),
                Some(id) if !r.kind.removes() && !self.chosen.contains_key(id) && !self.decided.contains(id) => {
                    let id = id.clone();
                    self.missing_item(&id, &r.rel)?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// A pending row whose object is gone: never sent, so nothing to take
    /// back. One the worker is sending right now is not taken from under it:
    /// a delete waits behind it, and learns the item id at its commit.
    fn gone_pending(&mut self, row: &OutboxRow) {
        if row.state != OutboxState::Running {
            self.ops.push(OutboxOp::Remove(row.seq));
            return;
        }
        self.detections.push(Detection {
            kind: OutboxKind::Delete,
            item_id: None,
            inode: row.inode.clone(),
            rel: row.rel.clone(),
            base: None,
            target_parent: None,
            target_name: None,
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        });
    }

    /// Item `id`, expected at `rel`, is not in the batch.
    fn missing_item(&mut self, id: &str, rel: &Path) -> Result<(), ExamineError> {
        let Some(base) = self.base_row(id)? else { return Ok(()) };
        // Save-by-rename: a new file now stands at its name — not one the
        // worker is creating right now.
        if base.kind == Kind::File {
            if let Some(&s) = self.at.get(rel) {
                let e = &self.entries[s];
                if e.ty == Type::File && e.id.is_none() && !self.consumed.contains(&s) && !self.being_created(e) {
                    self.consumed.insert(s);
                    self.chosen.insert(id.to_owned(), s);
                    return self.save_by_rename(id, &base, s);
                }
            }
        }
        let Some(handle) = self.local_handle(id)? else {
            // A rebuilt base cannot prove a delete (WR4).
            self.hold_back(id, Settle::Unproven, true);
            return Ok(());
        };
        match self.place_of(&handle) {
            // `ESTALE` is a delete only with its evidence: nothing, or another object, at its
            // place.
            Place::Gone if self.absent(rel, &handle) => self.removal(OutboxKind::Delete, id, &base, rel, None, None).map(drop),
            // Undecided: asked again at its place after [`RECHECK`], never
            // forgotten — nothing else would bring it back to an examination.
            Place::Gone | Place::Unknown => {
                self.recheck_at(rel);
                self.hold_back(id, Settle::Wait, true);
                Ok(())
            }
            Place::Outside(to) => self.removal(OutboxKind::MoveOut, id, &base, rel, Some(object(handle)), Some(to.as_path())).map(drop),
            // Moved within the folder, somewhere this batch did not look: found
            // there, or — gone by then — missing again from here.
            Place::Inside(now) => {
                self.recheck_at(&now);
                self.recheck_at(rel);
                self.hold_back(id, Settle::Wait, false);
                Ok(())
            }
        }
    }

    /// Item `id` leaves OneDrive (`delete`, or `move-out` to `went_to`). A
    /// folder takes along what the base has inside it — except what left it
    /// first, which leaves on its own and is ordered in front of it (rule
    /// 3). Every item still with the folder is asked where it is (§3.4 rule
    /// 7: by the object, not the events); while any cannot be placed, the
    /// folder waits. Rows that already say an item left are kept. What never
    /// reached the cloud goes; an item moved in from elsewhere, which the
    /// cloud has elsewhere, leaves by its own object. Rows the worker is
    /// running are never removed; a moved-in item's gets its follow-up.
    /// Whether its row was written, or why not.
    pub(super) fn removal(&mut self, kind: OutboxKind, id: &str, base: &Row, rel: &Path, inode: Option<Inode>, went_to: Option<&Path>) -> Result<Settle, ExamineError> {
        if self.decided.contains(id) {
            return Ok(self.deferred.get(id).copied().unwrap_or(Settle::Done));
        }
        self.decided.insert(id.to_owned());
        if base.kind == Kind::Folder {
            match self.left_before(id, rel, went_to)? {
                Settle::Done => {}
                // Something inside is elsewhere in the folder, or cannot be
                // placed: the folder is examined again, and removed then.
                Settle::Wait => {
                    self.hold_back(id, Settle::Wait, true);
                    self.recheck_at(rel);
                    return Ok(Settle::Wait);
                }
                // Something inside has no recorded handle: nothing can prove
                // it gone until the reconcile places it again.
                Settle::Unproven => {
                    self.hold_back(id, Settle::Unproven, true);
                    return Ok(Settle::Unproven);
                }
            }
            let inside: HashSet<String> = self.store({ let id = id.to_owned(); move |s| s.descendants(Table::Items, &id) })?.into_iter().collect();
            let mut rows: Vec<OutboxRow> = self.rows.under(rel).into_iter().cloned().collect();
            let mut taken: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
            for id in &inside {
                rows.extend(self.rows.of_item(id).filter(|r| taken.insert(r.seq)).cloned());
            }
            rows.sort_by_key(|r| r.seq);
            for r in rows {
                if r.state == OutboxState::Running {
                    match r.item_id.clone() {
                        // A create under its path had its object go with it.
                        None if is_under(&r.rel, rel) => self.gone_pending(&r),
                        // Moved in from elsewhere while being sent: it leaves
                        // by its own object, behind the running row.
                        Some(item) if !inside.contains(&item) && !self.chosen.contains_key(&item) && !self.decided.contains(&item) => {
                            self.missing_item(&item, &r.rel)?;
                        }
                        // What the base has inside waits in front of the
                        // folder (rule 3).
                        _ => {}
                    }
                    continue;
                }
                match r.item_id.clone() {
                    None => self.ops.push(OutboxOp::Remove(r.seq)),
                    Some(item) if self.chosen.contains_key(&item) || self.decided.contains(&item) => {}
                    // Left before the folder: kept.
                    Some(_) if r.kind == OutboxKind::MoveOut => {}
                    Some(item) if inside.contains(&item) => {
                        // Gone with the folder in the cloud too — unless the
                        // row takes it elsewhere in the folder.
                        if r.kind == OutboxKind::Delete || is_under(&r.rel, rel) {
                            self.ops.push(OutboxOp::Remove(r.seq));
                        }
                    }
                    Some(_) if r.kind == OutboxKind::Delete => {}
                    Some(item) => self.missing_item(&item, &r.rel)?,
                }
            }
        }
        self.detections.push(Detection {
            kind,
            item_id: Some(id.to_owned()),
            inode,
            rel: rel.to_path_buf(),
            base: Some(base_of(base)),
            target_parent: None,
            // Where a move out went, proved: what a later `ESTALE` is checked against.
            target_name: went_to.filter(|_| kind == OutboxKind::MoveOut).and_then(place_name).map(str::to_owned),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        });
        Ok(Settle::Done)
    }

    /// Items the base has inside `folder` (at `rel`), which is leaving: each
    /// is asked where it is, top down, unless this batch decided it or a row
    /// of its own already takes it elsewhere or out. One alive outside the
    /// folder (and not where the folder went, `went_to`) left it first: its
    /// own `move-out`, which downloads it before anything is deleted (WR5).
    /// One gone went with the folder, and so did one under `went_to`; what is
    /// inside those is asked too. How the folder may go: one alive elsewhere
    /// in the folder or unplaceable keeps it waiting, one with no
    /// recorded handle keeps it unproven, and so does an item held back
    /// on its own — a subfolder that left and waits itself.
    fn left_before(&mut self, folder: &str, rel: &Path, went_to: Option<&Path>) -> Result<Settle, ExamineError> {
        let mut settled = Settle::Done;
        let mut queue: VecDeque<String> = VecDeque::from([folder.to_owned()]);
        while let Some(parent) = queue.pop_front() {
            for child in self.store({ let parent = parent.to_owned(); move |s| s.children(Table::Items, &parent) })? {
                if child.placement != Placement::Placed {
                    continue;
                }
                let id = child.id.clone();
                self.base.entry(id.clone()).or_insert_with(|| Some(child.clone()));
                if self.chosen.contains_key(&id) {
                    continue;
                }
                if self.decided.contains(&id) {
                    settled = settled.max(self.deferred.get(&id).copied().unwrap_or(Settle::Done));
                    continue;
                }
                let Expect::At(at) = self.expected(&id)? else { continue };
                if !is_under(&at, rel) {
                    // A row of its own takes it elsewhere.
                    continue;
                }
                let Some(handle) = self.local_handle(&id)? else {
                    settled = settled.max(Settle::Unproven);
                    continue;
                };
                match self.place_of(&handle) {
                    Place::Outside(to) if went_to.is_none_or(|went| !to.starts_with(went)) => {
                        let left = self.removal(OutboxKind::MoveOut, &id, &child, &at, Some(object(handle)), Some(to.as_path()))?;
                        settled = settled.max(left);
                    }
                    // Gone with the folder only with its evidence, where the folder is now.
                    Place::Gone if !self.absent_with(&at, rel, went_to, &handle) => settled = settled.max(Settle::Wait),
                    Place::Gone | Place::Outside(_) => {
                        if child.kind == Kind::Folder {
                            queue.push_back(id);
                        }
                    }
                    Place::Inside(now) => {
                        self.recheck_at(&now);
                        settled = settled.max(Settle::Wait);
                    }
                    Place::Unknown => settled = settled.max(Settle::Wait),
                }
            }
        }
        Ok(settled)
    }

    /// Whether `dir` is, or is inside, a directory without an item id whose
    /// name is ignored: what is in it stays local (the outbox on the bus). A folder
    /// from OneDrive syncs whatever its name.
    pub(super) fn in_ignored_dir(&self, dir: &Path) -> bool {
        let mut at = dir;
        while let (Some(name), Some(parent)) = (at.file_name(), at.parent()) {
            if self.ex.ignore.matches(name) {
                let managed = self
                    .ex
                    .disk
                    .dir(parent)
                    .and_then(|d| self.ex.disk.probe(&d, name))
                    .map(|probe| matches!(probe, crate::folder::disk::Probe::Managed { .. }));
                // A directory the worker is making right now is an item
                // already, whatever its name (the outbox on the bus).
                if matches!(managed, Ok(false)) && !self.dir_being_made(at) {
                    return true;
                }
            }
            at = parent;
        }
        false
    }

    /// Whether the worker is making the directory at `rel` in OneDrive right
    /// now: a running `mkdir` of its object.
    fn dir_being_made(&self, rel: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        if self.rows.making.is_empty() {
            return false;
        }
        let Ok(meta) = self.ex.disk.dir(rel).and_then(|dir| dir.metadata()) else { return false };
        self.rows.making.iter().map(|&i| &self.rows.all[i]).any(|row| row.inode.as_ref().is_some_and(|i| i.dev == meta.dev() && i.ino == meta.ino()))
    }
}
