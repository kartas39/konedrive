use std::collections::{HashSet, VecDeque};
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{is_under, place_name, Inode, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use konedrive_tree::{Kind, Row};

use super::decisions::Settle;
use super::detect::leaves;
use super::facts::Expect;
use super::{base_of, object, ExamineError, Place, Run};
use crate::local::entry::{Entry, Type};

/// The objects of the entries listed, to tell whether a pending row's object
/// was seen ([`Inode::same_object`]) without comparing it with every entry.
struct Objects {
    handles: HashSet<FileHandle>,
    /// Every entry's inode, and those of entries with no handle.
    inodes: HashSet<(u64, u64)>,
    unhandled: HashSet<(u64, u64)>,
}

impl Objects {
    fn of(entries: &[Entry]) -> Self {
        let mut objects = Objects { handles: HashSet::new(), inodes: HashSet::new(), unhandled: HashSet::new() };
        for e in entries {
            objects.inodes.insert((e.dev, e.ino));
            match &e.handle {
                Some(handle) => {
                    objects.handles.insert(handle.clone());
                }
                None => {
                    objects.unhandled.insert((e.dev, e.ino));
                }
            }
        }
        objects
    }

    fn seen(&self, inode: &Inode) -> bool {
        match &inode.handle {
            Some(handle) => self.handles.contains(handle) || self.unhandled.contains(&(inode.dev, inode.ino)),
            None => self.inodes.contains(&(inode.dev, inode.ino)),
        }
    }
}

impl Run<'_, '_, '_> {
    /// Rule 7: base items (and pending rows) expected in the examined
    /// places and not found anywhere in the batch.
    pub(super) fn missing(&mut self) -> Result<(), ExamineError> {
        let objects = Objects::of(self.listing.entries());
        for (dir, _) in self.listing.places() {
            let mut items: Vec<(String, std::path::PathBuf)> = Vec::new();
            if let Some(parent) = self.dir_id(&dir) {
                for child in self.facts.children(&parent)? {
                    if let Expect::At(rel) = self.facts.expected(&child.id)? {
                        if rel.parent() == Some(dir.as_path()) && self.examined(&rel) {
                            items.push((child.id.clone(), rel));
                        }
                    }
                }
            }
            let mut pending: Vec<OutboxRow> = Vec::new();
            let listed: HashSet<String> = items.iter().map(|(id, _)| id.clone()).collect();
            for row in self.facts.rows.in_dir(&dir) {
                if row.kind.removes() || !self.examined(&row.rel) {
                    continue;
                }
                match &row.item_id {
                    Some(id) => {
                        if !listed.contains(id) && self.facts.rows.of_item(id).next_back().map(|r| r.seq) == Some(row.seq) {
                            items.push((id.clone(), row.rel.clone()));
                        }
                    }
                    None => pending.push(row.clone()),
                }
            }
            for (id, rel) in items {
                if self.decisions.open(&id) {
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
        let inside: Vec<OutboxRow> = self.facts.rows.under(&row.rel).into_iter().cloned().collect();
        for r in inside {
            match &r.item_id {
                None => self.gone_pending(&r),
                Some(id) if !r.kind.removes() && self.decisions.open(id) => {
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
            self.outcome.ops.push(OutboxOp::Remove(row.seq));
            return;
        }
        self.outcome.detections.push(leaves(OutboxKind::Delete, None, None, row.inode.clone(), &row.rel, None));
    }

    /// Item `id`, expected at `rel`, is not in the batch.
    fn missing_item(&mut self, id: &str, rel: &Path) -> Result<(), ExamineError> {
        let Some(base) = self.facts.row(id)? else { return Ok(()) };
        // Save-by-rename: a new file now stands at its name — not one the
        // worker is creating right now.
        if base.kind == Kind::File {
            if let Some(s) = self.listing.at(rel) {
                let e = &self.listing[s];
                if e.ty == Type::File && self.decisions.id_of(self.listing, s).is_none() && !self.decisions.taken(s) && !self.facts.rows.being_created(e) {
                    self.decisions.is_item(id, s);
                    return self.save_by_rename(id, &base, s);
                }
            }
        }
        let Some(handle) = self.facts.recorded(id)? else {
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
        if let Some(settled) = self.decisions.settled(id) {
            return Ok(settled);
        }
        self.decisions.settle(id, Settle::Done);
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
            let inside: HashSet<String> = self.facts.descendants(id)?.into_iter().collect();
            let mut rows: Vec<OutboxRow> = self.facts.rows.under(rel).into_iter().cloned().collect();
            let mut taken: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
            for id in &inside {
                rows.extend(self.facts.rows.of_item(id).filter(|r| taken.insert(r.seq)).cloned());
            }
            rows.sort_by_key(|r| r.seq);
            for r in rows {
                if r.state == OutboxState::Running {
                    match r.item_id.clone() {
                        // A create under its path had its object go with it.
                        None if is_under(&r.rel, rel) => self.gone_pending(&r),
                        // Moved in from elsewhere while being sent: it leaves
                        // by its own object, behind the running row.
                        Some(item) if !inside.contains(&item) && self.decisions.open(&item) => {
                            self.missing_item(&item, &r.rel)?;
                        }
                        // What the base has inside waits in front of the
                        // folder (rule 3).
                        _ => {}
                    }
                    continue;
                }
                match r.item_id.clone() {
                    None => self.outcome.ops.push(OutboxOp::Remove(r.seq)),
                    Some(item) if !self.decisions.open(&item) => {}
                    // Left before the folder: kept.
                    Some(_) if r.kind == OutboxKind::MoveOut => {}
                    Some(item) if inside.contains(&item) => {
                        // Gone with the folder in the cloud too — unless the
                        // row takes it elsewhere in the folder.
                        if r.kind == OutboxKind::Delete || is_under(&r.rel, rel) {
                            self.outcome.ops.push(OutboxOp::Remove(r.seq));
                        }
                    }
                    Some(_) if r.kind == OutboxKind::Delete => {}
                    Some(item) => self.missing_item(&item, &r.rel)?,
                }
            }
        }
        // Where a move out went, proved: what a later `ESTALE` is checked against.
        let went_to = went_to.filter(|_| kind == OutboxKind::MoveOut).and_then(place_name).map(str::to_owned);
        self.outcome.detections.push(leaves(kind, Some(id), Some(base_of(base)), inode, rel, went_to));
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
            for child in self.facts.children(&parent)? {
                let id = child.id.clone();
                if self.decisions.found(&id) {
                    continue;
                }
                if let Some(settle) = self.decisions.settled(&id) {
                    settled = settled.max(settle);
                    continue;
                }
                let Expect::At(at) = self.facts.expected(&id)? else { continue };
                if !is_under(&at, rel) {
                    // A row of its own takes it elsewhere.
                    continue;
                }
                let Some(handle) = self.facts.recorded(&id)? else {
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
}
