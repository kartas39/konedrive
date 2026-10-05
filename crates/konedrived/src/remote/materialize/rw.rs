//! The reconcile in read-write mode (`docs/design/writes.md` §9, §7).
//!
//! The read phase makes the folder match the tree; in read-write mode the
//! folder also holds the user's own changes, which the outbox sends. So the
//! reconcile here:
//!
//! - **keeps its hands off what a local change holds** ([`Rw::held`]): an
//!   item with a live outbox row, in any state, and what the base has below
//!   a folder such a row moves — not moved, replaced or removed. Nor what it
//!   finds away from where the base has it, a local move or copy not
//!   examined yet, with everything below it. Their changes wait
//!   ([`Pending::unsettled`](super::Pending::unsettled)): the base keeps the
//!   version the disk holds;
//! - **places nothing below a folder being removed** ([`Rw::removing`]):
//!   below a live `delete` or `move-out` row the delta's changes go to the
//!   base, and nothing goes to the disk (the read-write reconcile must, item 3);
//! - **places a missing item again only when there is something to place**
//!   ([`Rw::revive`]): new in OneDrive, or with no local object on record (a
//!   rebuilt base, one the outbox forgot, a restore). Anything else missing is
//!   a delete or a move the examination has still to see — a changed one too:
//!   its object may be alive out of the folder, and only once the
//!   examination and the outbox have decided (delete × edit: OneDrive wins,
//!   §6) is it placed again;
//! - **keeps both where the read phase rescued** (§6): a local version in the
//!   way is renamed beside the cloud's, `name-<machine>.ext`, and uploaded
//!   as new ([`Materializer::copy_aside`]);
//! - **removes what OneDrive removed, in the cycle, and keeps what it never
//!   had**: what OneDrive had and the daemon placed goes — a file not
//!   downloaded, a download unchanged since, a download into it stopped, a
//!   folder once nothing is left in it. A file made here and a download
//!   changed here stay, with their folders, their attributes off, and go up
//!   as new; nothing is rescued out of the folder
//!   (`resyncChangesUploadDifferences` keeps every download too, §3.7);
//! - **lets what can no longer be placed wait where it is** (issue #104):
//!   an item OneDrive still has and the folder cannot hold goes from the
//!   disk whole, in the cycle, when nothing in it waits; while anything
//!   does it stays an item like any other, placed by the base, and its
//!   change waits with what it waits for ([`Policy::Unplaced`],
//!   [`Materializer::wait_to_leave`]);
//! - **forgets before it removes**: the local objects of everything it takes
//!   off the disk are forgotten in the store first, so that no examination
//!   can prove one gone and delete it in OneDrive
//!   ([`Materializer::take_off`], the only way anything is taken off);
//! - **never moves anything out of the folder**: what it moved to the holding
//!   directory — this run, or one a stop cut short — is placed from there or
//!   put back where it was, never rescued outside, where it would be taken
//!   for a move out and deleted in OneDrive.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder;

use super::removal::Policy;
use super::{ApplyError, Copied, Materializer, Run};
use crate::folder::disk::{Probe, HOLDING, NEW_PREFIX};
use crate::local::IgnoreList;
use crate::local::names::copy_name;
use konedrive_tree::outbox::OutboxOp;
use konedrive_tree::{Kind, Planned, Table, TreeError, TreeStore};

/// The holding directory in read-write mode: nothing in it leaves the folder.
mod holding;
/// Phase 1: what is done with each object of ours.
mod sort;
pub(super) use sort::{no_longer_placed, Scan};
/// What can no longer be placed: it goes or waits whole, and yields its name.
mod unplaced;
pub(super) use unplaced::Unplaced;

/// What a read-write folder's reconcile needs to know besides the tree.
#[derive(Debug, Default, Clone)]
pub struct Rw {
    /// Items a live outbox row concerns, and what the base or the new tree
    /// has below a folder such a row moves: hands off, their changes wait.
    pub held: HashSet<String>,
    /// Items a live `delete` or `move-out` row removes, and everything below
    /// them: nothing is placed there, and their changes go to the base.
    pub removing: HashSet<String>,
    /// Where rows with no item id stand — a `create` or a `mkdir` not landed
    /// yet: an object there without an id is the outbox's, not in the way.
    /// Each such place and every folder above it, so that a look is one
    /// lookup (issue #39).
    pub pending: HashSet<PathBuf>,
    /// Items placed again where missing: new in OneDrive, a file whose
    /// content changed there, an item with no local object on record — and
    /// every folder above them.
    pub revive: HashSet<String>,
    /// Items the base records no local object for (F82 (8)).
    pub unplaced: HashSet<String>,
    /// Items the new tree changes: what `staging` differs from `items` by.
    pub changed: HashSet<String>,
    /// For conflict copies: `name-<machine>.ext` (§6).
    pub machine: String,
    /// `resyncChangesUploadDifferences` (§3.7): what OneDrive's new listing
    /// left out and was downloaded here is uploaded again as new, and a
    /// downloaded file that differs from OneDrive's version is kept beside it.
    pub upload_differences: bool,
    /// The account's ignore list: what a removal keeps under an ignored
    /// name stays on this computer only.
    pub ignore: IgnoreList,
}

impl Rw {
    /// Read from the store once `staging` holds the new tree: the outbox's
    /// live rows, and what the new tree changes.
    pub fn read(s: &TreeStore, machine: String, upload_differences: bool, ignore: IgnoreList) -> Result<Self, TreeError> {
        let mut rw = Rw { machine, upload_differences, ignore, ..Rw::default() };
        for row in s.outbox_rows()? {
            let Some(id) = row.item_id.clone() else {
                rw.pending.extend(row.rel.ancestors().map(Path::to_path_buf));
                continue;
            };
            let folder = s.get(Table::Items, &id)?.is_some_and(|r| r.kind == Kind::Folder);
            if folder {
                // What the base has below the folder goes with the row; what
                // OneDrive moved in from elsewhere since is not the folder's:
                // the base still has it where it was, and its move waits.
                rw.extend_below(s, &id, row.kind.removes())?;
            }
            if row.kind.removes() {
                rw.removing.insert(id);
            } else {
                rw.held.insert(id);
            }
        }
        rw.unplaced = s.unplaced(Table::Staging)?.into_iter().collect();
        let mut wanted: Vec<String> = rw.unplaced.iter().cloned().collect();
        rw.changed = s.changed_ids()?.into_iter().collect();
        for id in &rw.changed {
            let id = id.clone();
            let Some(row) = s.get(Table::Staging, &id)? else { continue };
            let new = match s.get(Table::Items, &id)? {
                None => true,
                Some(base) => row.kind == Kind::File && (row.ctag != base.ctag || row.size != base.size),
            };
            if new {
                wanted.push(id);
            }
        }
        // Each with the folders above it, up to the root.
        let mut parents: HashMap<String, Option<String>> = HashMap::new();
        for id in wanted {
            let mut at = Some(id);
            let mut depth = 0;
            while let Some(id) = at.take() {
                if !rw.revive.insert(id.clone()) || depth > konedrive_fs::MAX_DEPTH {
                    break;
                }
                depth += 1;
                let parent = match parents.get(&id) {
                    Some(parent) => parent.clone(),
                    None => {
                        let parent = s.get(Table::Staging, &id)?.and_then(|r| r.parent_id);
                        parents.insert(id.clone(), parent.clone());
                        parent
                    }
                };
                at = parent;
            }
        }
        Ok(rw)
    }

    /// Everything below folder `id`, which a live row takes away (`removing`)
    /// or moves: the base's descendants go with the row; one the new tree
    /// moves in from elsewhere in the base is held where the base has it, its
    /// move waiting; one new to the base goes with the row too.
    fn extend_below(&mut self, s: &TreeStore, id: &str, removing: bool) -> Result<(), TreeError> {
        let base: HashSet<String> = s.descendants(Table::Items, id)?.into_iter().collect();
        for staged in s.descendants(Table::Staging, id)? {
            if base.contains(&staged) {
                continue;
            }
            if s.get(Table::Items, &staged)?.is_some() {
                self.held.insert(staged);
            } else if removing {
                self.removing.insert(staged);
            } else {
                self.held.insert(staged);
            }
        }
        if removing {
            self.removing.extend(base);
        } else {
            self.held.extend(base);
        }
        Ok(())
    }

    /// Whether a row with no item id stands at `rel`, or below it.
    pub(super) fn pending_at(&self, rel: &Path) -> bool {
        self.pending.contains(rel)
    }

    /// How what OneDrive removed is taken off the disk: what was changed
    /// or made here stays, and after a `resyncChangesUploadDifferences`
    /// listing, which does not mean removed, every download too (§3.7).
    pub(super) fn removed(&self) -> Policy {
        if self.upload_differences {
            Policy::Resync
        } else {
            Policy::Removed
        }
    }

    /// Whether the cloud has something to put at item `id`'s name that the
    /// disk does not show: the new tree changes it, or no local object of it
    /// is on record. Otherwise an object of the user's there — a save by
    /// rename, say — is theirs until the examination sees it (§3.4 rule 7).
    pub(super) fn brings(&self, id: &str) -> bool {
        self.changed.contains(id) || self.unplaced.contains(id)
    }
}

/// Whether `rel` is directly in the holding directory.
fn in_holding(rel: &Path) -> bool {
    rel.parent() == Some(Path::new(HOLDING))
}

/// Whether `rel`'s name is a new folder's or a replacement's temporary one.
fn is_new_name(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(NEW_PREFIX.as_bytes()))
}

impl Materializer {
    /// Whether another item of ours at `rel` is there by a local change —
    /// with a row, below one, or away from where the base has it — so that
    /// it keeps the name: the outbox settles the two (§6).
    pub(super) fn holds_the_name(&self, rw: &Rw, other: &str, rel: &Path, run: &Run) -> Result<bool, ApplyError> {
        if rw.held.contains(other) || rw.removing.contains(other) || run.left.contains(other) {
            return Ok(true);
        }
        let base = self.store.call_blocking({ let other = other.to_owned(); move |s| s.locate(Table::Items, &other) })?;
        Ok(!base.is_some_and(|l| l.placed && l.rel == rel))
    }

    /// Whether `row`, missing from its place, is made again: only with
    /// something to place ([`Rw::revive`]), and never while its local object
    /// is on record. Such an object may still be alive:
    /// elsewhere in the folder, or out of it — a placeholder moved out, which
    /// only the move-out step's `move-out` row marks and downloads; placed again here, the
    /// item would be found at its place and the object outside would read
    /// zeros for good. Its base place goes to the examination instead, which
    /// decides by the object; the outbox then meets OneDrive's change (a
    /// delete or a move out answered `412` is dropped, its object forgotten)
    /// and the next cycle places the item again.
    pub(super) fn place_again(&self, rw: &Rw, row: &konedrive_tree::Row, rel: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        if !rw.revive.contains(&row.id) {
            return Ok(false);
        }
        if self.store.call_blocking({ let row_id = row.id.clone(); move |s| s.local_handle(&row_id) })?.is_some() {
            let base = self.store.call_blocking({ let row_id = row.id.clone(); move |s| s.locate(Table::Items, &row_id) })?.filter(|l| l.placed).map(|l| l.rel);
            run.out.on_disk.examine.push((base.unwrap_or_else(|| rel.to_path_buf()), false));
            return Ok(false);
        }
        Ok(true)
    }

    /// Whether the place the new tree gives item `id` is held by a local
    /// change: another item's object there that a row holds, that the base
    /// does not have there, or that this run left; a file or folder made
    /// here whose row waits; or another item's object that is itself to
    /// move and whose own new place is held — a chain of names that hangs
    /// on a local change, followed to its end. The item is then left where
    /// it is, with what is below it, and its move waits: taken to the
    /// holding directory it could only be put back, and under another name
    /// if its own was taken meanwhile. Asked of the disk and the plan, not
    /// of what this run did so far, so the answer does not depend on the
    /// order the items are looked at in.
    pub(super) fn destination_held(&self, rw: &Rw, id: &str, planned: &Planned, run: &Run) -> Result<bool, ApplyError> {
        self.held_from(rw, id, planned, run, &mut HashSet::new())
    }

    fn held_from(&self, rw: &Rw, id: &str, planned: &Planned, run: &Run, seen: &mut HashSet<String>) -> Result<bool, ApplyError> {
        // Names that only go round (an exchange) hang on nothing.
        if !seen.insert(id.to_owned()) {
            return Ok(false);
        }
        let Some(to) = planned.new_place() else { return Ok(false) };
        let Some(name) = to.rel.file_name() else { return Ok(false) };
        let Ok(dir) = self.disk.dir(to.rel.parent().unwrap_or(Path::new(""))) else { return Ok(false) };
        Ok(match self.disk.probe(&dir, name)? {
            Probe::Managed { id: other, .. } if other != id && !run.leaving.contains(&other) => {
                if self.holds_the_name(rw, &other, &to.rel, run)? {
                    return Ok(true);
                }
                let theirs = self.store.call_blocking({ let other = other.clone(); move |s| s.plan(&[other]) })?;
                let theirs = theirs.of(&other);
                // It stands where the base has it: held if the tree moves it
                // on to a place that is held.
                theirs.new_place().is_some_and(|next| next.rel != to.rel) && self.held_from(rw, &other, theirs, run, seen)?
            }
            Probe::Unmanaged { .. } => rw.pending_at(&to.rel),
            _ => false,
        })
    }

    /// `id` and everything the base or the new tree has below it wait.
    pub(super) fn unsettle_tree(&self, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        run.out.pending.unsettled.insert(id.to_owned());
        let folder = id.to_owned();
        let below = self.store.call_blocking(move |s| {
            let mut ids = s.descendants(Table::Staging, &folder)?;
            ids.extend(s.descendants(Table::Items, &folder)?);
            Ok(ids)
        })?;
        run.out.pending.unsettled.extend(below);
        Ok(())
    }

    /// Keeps both (§6): the object at `dir/name` (at `rel`) is renamed in its
    /// directory to the first free `name-<machine>.ext`, never over anything,
    /// and becomes the user's own — konedrive's attributes off, the item id
    /// first ([`placeholder::strip`]) — to be uploaded as new; the name is the cloud's again. Rows below a directory
    /// follow it.
    pub(super) fn copy_aside(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let original = name.to_str().ok_or_else(|| ApplyError::Io(format!("{} has a name that is not UTF-8", rel.display())))?;
        let is_dir = matches!(self.disk.probe(dir, name)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        let mut copy = None;
        for n in 1..=100 {
            let candidate = copy_name(original, &rw.machine, n);
            match self.disk.rename(dir, name, dir, OsStr::new(&candidate)) {
                Ok(()) => {
                    copy = Some(candidate);
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let copy = copy.ok_or_else(|| ApplyError::Io(format!("no free name for a copy of {}", rel.display())))?;
        let copied = OsStr::new(&copy);
        if is_dir {
            placeholder::strip(&self.disk.open_subdir(dir, copied)?)?;
        } else if let Ok(file) = self.disk.open_file(dir, copied) {
            placeholder::strip(&file)?;
        }
        let copy_rel = rel.with_file_name(&copy);
        if is_dir {
            let rebase = [OutboxOp::Rebase { from: rel.to_path_buf(), to: copy_rel.clone() }];
            self.store.call_blocking(move |s| s.outbox_apply(&rebase, 0))?;
        }
        tracing::info!("{} changed here and in OneDrive: the local version is kept as {}", rel.display(), copy_rel.display());
        run.out.on_disk.copies.push(Copied { original: rel.to_path_buf(), copy: copy_rel.clone() });
        run.out.on_disk.examine.push((copy_rel, is_dir));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
