//! Phase 1 in a read-write folder: what is done with each object of ours,
//! by where the base has its item, where the new tree has it, and what a
//! local change holds ([`Sorted`](crate::remote::materialize::Sorted)).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use konedrive_tree::outbox::SWAP_PREFIX;
use konedrive_tree::{Kind, Located, Placement, Planned, WaitsFor};

use super::{in_holding, is_new_name, Rw};
use crate::folder::disk::{Probe, Scanned};
use crate::remote::materialize::{is_leftover_replacement, is_misplaced, ApplyError, Materializer, Run, Seen, Sorted};

/// Where a misplaced entry of the Full scan was, by the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Was {
    /// At its base place, and the new tree places it elsewhere: moved in
    /// OneDrive — or not the base's at all, as the read phase takes it.
    Moved,
    /// At its base place, and the new tree does not have it: removed in
    /// OneDrive.
    Removed,
    /// At its base place, and the new tree has it but does not place it: no
    /// longer placeable here (issue #104).
    Unplaced,
    /// Away from its base place: a local move or copy not examined yet.
    Elsewhere,
    /// Under the outbox's temporary name in OneDrive (F82 (5)): the local
    /// object stays where it is, and the examination moves the item back.
    Swapped,
    /// An id neither the base nor the new tree has: not ours to remove.
    Stranger,
}

/// Whether the new tree has the item under the outbox's temporary name.
fn swapped(planned: &Planned) -> bool {
    planned.new.as_ref().is_some_and(|new| new.row.name.starts_with(SWAP_PREFIX))
}

/// Where the misplaced `entry` was, by its item's plan. It reads nothing and
/// changes nothing.
pub(super) fn where_it_was(entry: &Scanned, planned: &Planned) -> Was {
    if swapped(planned) {
        return Was::Swapped;
    }
    let (staged, placed) = (planned.new.is_some(), planned.new_place().is_some());
    let Some(base) = &planned.base else {
        // Not the base's: one this very placement left (a cycle stopped
        // before its swap) is put where the tree has it; anything else is
        // a file from elsewhere — another folder, another account — the
        // user's, which the examination takes as new (§3.4 rule 6).
        return if staged && placed { Was::Moved } else { Was::Stranger };
    };
    let at_base_place = base.row.parent_id == entry.parent_id && entry.rel.file_name() == Some(OsStr::new(&base.row.name)) && (base.row.kind == Kind::Folder) == entry.is_dir;
    if !base.placed() {
        // Not placed by the base, and here all the same: what a cycle that
        // stopped left. Where the base has it, it is no longer placeable;
        // anywhere else the examination decides first.
        return match (staged, placed) {
            (false, _) => Was::Removed,
            (true, false) if at_base_place => Was::Unplaced,
            // Placed by the tree, and not by the base: as for anything
            // away from its base place, the examination decides first.
            (true, _) => Was::Elsewhere,
        };
    }
    if base.row.placement != Placement::Placed || !at_base_place {
        return Was::Elsewhere;
    }
    match (staged, placed) {
        (true, true) => Was::Moved,
        (true, false) => Was::Unplaced,
        (false, _) => Was::Removed,
    }
}

/// Whether OneDrive still has the item and the new tree no longer places
/// it (issue #104): not one under the outbox's temporary name there, whose
/// object stays where it is.
pub(in crate::remote::materialize) fn no_longer_placed(planned: &Planned) -> bool {
    planned.new.is_some() && planned.new_place().is_none() && !swapped(planned)
}

/// What the sorting of a Full scan keeps from one entry to those below it.
/// The scan lists a directory's entry before what is in it.
#[derive(Default)]
pub(in crate::remote::materialize) struct Scan {
    /// Directories a local change holds, with everything below them.
    left_dirs: HashSet<PathBuf>,
    /// Directories that can no longer be placed: what the scan lists below
    /// one follows it.
    unplaced_dirs: HashSet<PathBuf>,
}

impl Scan {
    /// The entry is left where it is, and so is what is below it. The item
    /// is left too — not placed, its change waiting — unless another object
    /// carries its id: a copy that kept the attributes, which the
    /// examination tells from the item.
    fn leave(&mut self, seen: &Seen, run: &mut Run) {
        if single(seen) {
            run.left.insert(seen.id.clone());
            // Its change waits too, whatever the tree does with it: the
            // examination takes the local move on, and the outbox meets
            // OneDrive's side (§6).
            run.out.pending.unsettled.insert(seen.id.clone());
        }
        if seen.entry.is_dir {
            self.left_dirs.insert(seen.entry.rel.clone());
        }
    }

    fn below(dirs: &HashSet<PathBuf>, entry: &Scanned) -> bool {
        entry.rel.ancestors().skip(1).any(|above| dirs.contains(above))
    }
}

/// Whether one object only carries the entry's id.
fn single(seen: &Seen) -> bool {
    seen.counts.get(seen.id.as_str()).copied().unwrap_or(0) <= 1
}

impl Materializer {
    /// The Full scope: what moved in OneDrive goes to the holding
    /// directory, what OneDrive removed goes where it stands, what can no
    /// longer be placed leaves once the rest is placed, and what a local
    /// change holds, or explains, is left with everything below it.
    pub(in crate::remote::materialize) fn sort_scanned_rw(&self, rw: &Rw, seen: &Seen, scan: &mut Scan, run: &mut Run) -> Result<Sorted, ApplyError> {
        let (entry, id) = (seen.entry, seen.id);
        // What a reconcile moved to the holding directory — this one's, or
        // one a stop or a crash cut short — is the daemon's own: the
        // placement takes it from there, or the drain puts it back where
        // it was. Never a local move, never out of the folder.
        if in_holding(&entry.rel) {
            return Ok(Sorted::Stays);
        }
        // A new folder a stop left under its temporary name goes to the
        // holding directory like anything misplaced: placed from there, or
        // drained. A replacement's leftover link (a file) is the read
        // phase's to recognise, next to its real file.
        if entry.is_dir && is_new_name(&entry.rel) && single(seen) {
            return Ok(Sorted::ToHolding);
        }
        let held = rw.held.contains(id) || rw.removing.contains(id);
        if Scan::below(&scan.unplaced_dirs, entry) {
            // What OneDrive moved out of it to where the tree places it is
            // moved, as anywhere; the rest stays with it.
            let moved_out = is_misplaced(entry, seen.new) && where_it_was(entry, seen.plan.of(id)) == Was::Moved && !held && !self.destination_held(rw, id, seen.plan.of(id), run)?;
            return Ok(if moved_out { Sorted::ToHolding } else { Sorted::Follows });
        }
        if Scan::below(&scan.left_dirs, entry) {
            scan.leave(seen, run);
            return Ok(Sorted::Stays);
        }
        if is_leftover_replacement(entry, id, seen.counts) {
            return Ok(Sorted::Leftover);
        }
        if !entry.is_dir && is_new_name(&entry.rel) {
            return Ok(Sorted::Stays);
        }
        if !is_misplaced(entry, seen.new) {
            if held {
                scan.leave(seen, run);
            }
            return Ok(Sorted::Stays);
        }
        let planned = seen.plan.of(id);
        // What can no longer be placed, standing beside its base place
        // under a copy name of it: stepped aside before a stop of the
        // daemon. The base takes the name now.
        let mut was = where_it_was(entry, planned);
        if was == Was::Elsewhere && planned.new.is_some() && planned.new_place().is_none() {
            // Only where one object carries the id: with a copy of it
            // about, which is which is the examination's.
            if let Some(base) = planned.base_place().filter(|_| single(seen)) {
                if self.stepped_aside_before(rw, id, &base.rel)?.is_some_and(|aside| aside == entry.rel) {
                    was = Was::Unplaced;
                }
            }
        }
        Ok(match (was, held) {
            // Whatever a local change holds, what OneDrive removed goes.
            (Was::Removed, _) => Sorted::Removed,
            // What is no longer placed goes whole or waits whole, decided
            // once what OneDrive moved out of it is placed: one still in
            // the holding directory by then keeps its folder.
            (Was::Unplaced, _) => {
                run.leaving.insert(id.clone());
                if entry.is_dir {
                    scan.unplaced_dirs.insert(entry.rel.clone());
                }
                Sorted::Leaves { stands: Some((entry.rel.clone(), entry.is_dir)) }
            }
            (Was::Moved, false) if !self.destination_held(rw, id, planned, run)? => Sorted::ToHolding,
            (_, true) | (Was::Elsewhere | Was::Moved, false) => {
                scan.leave(seen, run);
                Sorted::Stays
            }
            (Was::Swapped | Was::Stranger, false) => {
                if entry.is_dir {
                    scan.left_dirs.insert(entry.rel.clone());
                }
                Sorted::Stays
            }
        })
    }

    /// The Changed scope: as the read phase's, but a disagreement a local
    /// change explains never turns it Full — an item not where the base has
    /// it (`old`) is left to the examination, and its change waits. Of what
    /// can no longer be placed (`unplaced`: where the base has each such
    /// item) only the topmost item of a subtree is looked at, and what is
    /// no longer placed below it follows it.
    pub(in crate::remote::materialize) fn sort_changed_rw(&self, rw: &Rw, id: &str, old: &Located, planned: &Planned, unplaced: &[&Located], run: &mut Run) -> Result<Sorted, ApplyError> {
        if rw.removing.contains(id) {
            return Ok(Sorted::Stays);
        }
        let placed_now = planned.new_place().is_some();
        if swapped(planned) {
            // Under the outbox's temporary name in OneDrive (F82 (5)): the
            // base says where it is there, and its object stays.
            run.out.on_disk.taken.insert(id.to_owned());
            return Ok(Sorted::Stays);
        }
        let unplaced_now = planned.new.is_some() && !placed_now;
        if unplaced_now && unplaced.iter().any(|above| above.rel != old.rel && old.rel.starts_with(&above.rel)) {
            return Ok(Sorted::Follows);
        }
        // From here on an item that can no longer be placed is the topmost
        // of its subtree: whatever this pass does with it, what follows it
        // goes its way.
        let untouched = || if unplaced_now { Sorted::Leaves { stands: None } } else { Sorted::Stays };
        // An item that can no longer be placed and is not where the base
        // has it may have stepped aside before a stop of the daemon.
        let mut stood = old.rel.clone();
        if unplaced_now {
            let there = self.disk.dir(stood.parent().unwrap_or(Path::new(""))).ok().zip(stood.file_name()).map(|(dir, name)| self.disk.probe(&dir, name));
            if !matches!(there, Some(Ok(Probe::Managed { id: ref there, .. })) if there == id) {
                if let Some(aside) = self.stepped_aside_before(rw, id, &stood)? {
                    stood = aside;
                }
            }
        }
        let held = rw.held.contains(id) && (stood == old.rel || self.store.call_blocking({ let id = id.to_owned(); move |s| s.outbox_for_item(&id) })?.iter().any(|row| !row.kind.removes()));
        let parent = stood.parent().unwrap_or(Path::new(""));
        let Some(name) = stood.file_name() else { return Ok(untouched()) };
        // Whether nothing is there for sure: a folder that cannot be
        // opened for another reason says nothing of what is in it.
        let (found, looked) = match self.disk.dir(parent) {
            Ok(dir) => (self.disk.probe(&dir, name)?, true),
            Err(e) => (Probe::Absent, matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR))),
        };
        let at_place = matches!(&found, Probe::Managed { id: found, .. } if found == id);
        // A local change holds it — unless OneDrive removed it and its
        // object is where the base has it: that goes, whatever holds it.
        // What is no longer placed is looked at whole: a change that
        // holds it is one more thing it waits for (issue #104).
        if held && (placed_now || !at_place) {
            run.out.pending.unsettled.insert(id.to_owned());
            if unplaced_now {
                // No longer placed, and the user's own move holds it.
                run.out.pending.waits.push((id.to_owned(), WaitsFor::Uploads(1).to_string()));
            }
            return Ok(untouched());
        }
        if !at_place {
            // Deleted or moved here, not examined yet. Where the tree
            // still places it, phase 2 decides; where it removes it, the
            // removal waits: the examination takes the local change
            // on, and the outbox meets OneDrive's side (§6).
            run.missing.insert(id.to_owned());
            // No longer placed, not here, and no object of it on record:
            // nothing an examination could still find. The base takes
            // the change.
            let nothing_left = looked && unplaced_now && self.store.call_blocking({ let id = id.to_owned(); move |s| s.local_handle(&id) })?.is_none();
            if nothing_left {
                run.out.on_disk.taken.insert(id.to_owned());
            } else if !placed_now {
                run.out.pending.unsettled.insert(id.to_owned());
                if planned.new.is_some() {
                    // Not where the base has it, to be examined; or its
                    // folder cannot be opened, and nothing is known.
                    let at = old.rel.display().to_string();
                    let waits = if looked { WaitsFor::Changes(at) } else { WaitsFor::UnknownState(at) };
                    run.out.pending.waits.push((id.to_owned(), waits.to_string()));
                }
            }
            return Ok(untouched());
        }
        if planned.stays() {
            return Ok(untouched());
        }
        Ok(match (&planned.new, placed_now) {
            (Some(_), true) if self.destination_held(rw, id, planned, run)? => {
                self.unsettle_tree(id, run)?;
                Sorted::Stays
            }
            (Some(_), true) => Sorted::ToHolding,
            // Once phase 2 has placed what OneDrive moved out of it.
            (Some(_), false) => {
                run.leaving.insert(id.to_owned());
                Sorted::Leaves { stands: Some((stood, matches!(found, Probe::Managed { is_dir: true, .. }))) }
            }
            (None, _) => Sorted::Removed,
        })
    }
}
