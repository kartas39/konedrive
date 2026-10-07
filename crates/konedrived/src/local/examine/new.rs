//! Rules 3 to 5: an entry without an item id — a new file or directory, unless it stays
//! local.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::Kind;

use super::listing::EntryIx;
use super::{ExamineError, Run};
use crate::folder::disk::Probe;
use crate::local::entry::Type;
use crate::local::names;

impl Run<'_, '_, '_> {
    pub(super) fn new_object(&mut self, ix: EntryIx) -> Result<(), ExamineError> {
        let e = &self.listing[ix];
        // At or inside a place the run gave up on (a directory it did not
        // strip of an id that is not its own: refused, or gone meanwhile):
        // it has no folder to go into.
        if self.listing.closed(e.dir_rel()) || self.decisions.gave_up(e.dir_rel()) {
            return Ok(());
        }
        let pending = self.facts.rows.pending(e).cloned();
        // 3. Ignored — its name, or a directory of the user's own above it
        // (the outbox on the bus): stays local, and a create it had goes — unless the
        // worker is creating it right now: then it is an item already, which
        // syncs whatever its name, and the rename waits behind.
        if self.ex.ignore.matches(&e.name) || self.in_ignored_dir(e.dir_rel()) {
            let being_created = self.facts.rows.being_created(e);
            if being_created {
                let moved = self.of_new(OutboxKind::Move, e);
                self.outcome.detections.push(moved);
            } else if let Some(row) = pending.filter(|r| matches!(r.kind, OutboxKind::Create | OutboxKind::Mkdir)) {
                self.outcome.ops.push(OutboxOp::Remove(row.seq));
            }
            // A directory newly ignored takes the new things waiting inside it
            // along: their folder is never made in OneDrive (the outbox on the bus).
            // One being made keeps them: it is an item already.
            if e.ty == Type::Dir && !being_created {
                let inside = self.facts.rows.under(&e.rel).into_iter().filter(|r| r.item_id.is_none() && r.state != OutboxState::Running).map(|r| OutboxOp::Remove(r.seq));
                self.outcome.ops.extend(inside);
            }
            return Ok(());
        }
        let is_dir = e.ty == Type::Dir;
        // A file over a name whose delete is still pending: save-by-rename
        // across batches (`docs/design/writes.md` §5.2).
        if !is_dir && pending.is_none() {
            let delete = self.facts.rows.at(&e.rel).find(|r| r.kind == OutboxKind::Delete && r.state != OutboxState::Running).and_then(|r| r.item_id.clone());
            if let Some(id) = delete {
                if let Some(base) = self.facts.row(&id)?.filter(|b| b.kind == Kind::File) {
                    self.decisions.is_link(ix);
                    return self.save_by_rename(&id, &base, ix);
                }
            }
        }
        let mut d = self.of_new(if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create }, e);
        if let Some(row) = &pending {
            if is_dir && row.rel != e.rel {
                self.outcome.ops.push(OutboxOp::Rebase { from: row.rel.clone(), to: e.rel.clone() });
            }
            if !is_dir && row.state == OutboxState::Running && row.snapshot_is(e.snapshot()) {
                // Being uploaded as it is now: only where it is matters.
                d.kind = OutboxKind::Move;
            }
        }
        let refused = names::refused(&e.name).or((!is_dir && e.size > names::MAX_FILE_SIZE).then_some(names::Refused::TooLarge));
        if let Some(refused) = refused {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.reason());
        } else if !is_dir {
            // One that cannot be opened is passed over, one that went is
            // not there: no row.
            let Some(ready) = self.probe_writer(e)? else { return Ok(()) };
            ready.onto(&mut d);
        }
        self.outcome.detections.push(d);
        Ok(())
    }

    /// Whether `dir` is, or is inside, a directory without an item id whose
    /// name is ignored: what is in it stays local (the outbox on the bus). A folder
    /// from OneDrive syncs whatever its name.
    fn in_ignored_dir(&self, dir: &Path) -> bool {
        let disk = self.ex.disk;
        let mut at = dir;
        while let (Some(name), Some(parent)) = (at.file_name(), at.parent()) {
            if self.ex.ignore.matches(name) {
                let managed = disk.dir(parent).and_then(|d| disk.probe(&d, name)).map(|probe| matches!(probe, Probe::Managed { .. }));
                // A directory the worker is making right now is an item
                // already, whatever its name (the outbox on the bus).
                let being_made = || self.facts.rows.being_made(|| disk.dir(at).and_then(|dir| dir.metadata()).ok().map(|meta| (meta.dev(), meta.ino())));
                if matches!(managed, Ok(false)) && !being_made() {
                    return true;
                }
            }
            at = parent;
        }
        false
    }
}
