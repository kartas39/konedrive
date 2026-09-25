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
//!   ([`Applied::unsettled`](super::Applied::unsettled)): the base keeps the
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
//! - **removes what OneDrive removed only where nothing local is lost**: a
//!   changed file stays, its konedrive attributes off, and is uploaded
//!   again; a folder that holds local work stays, and is made again in
//!   OneDrive (F82 (4)); one that holds only what is in use (or ignored)
//!   waits, keeping its id; a downloaded file goes only under a write lease;
//! - **never moves anything out of the folder**: what it moved to the holding
//!   directory — this run, or one a stop cut short — is placed from there or
//!   put back where it was, never rescued outside, where it would be taken
//!   for a move out and deleted in OneDrive.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{self, read_state, State};

use std::os::fd::AsFd;

use super::{is_leftover_replacement, ApplyError, Copied, Materializer, Run};
use crate::drive::item::RESERVED_PREFIX;
use crate::sync::activity::Kind as EventKind;
use crate::sync::disk::{Probe, Scanned, HOLDING, NEW_PREFIX};
use crate::sync::local::{names, IgnoreList};
use crate::sync::upload::copy_name;
use crate::tree::outbox::{is_under, OutboxOp, SWAP_PREFIX};
use crate::tree::{Kind, Placement, Table, TreeError, TreeStore};

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
    pub pending: Vec<PathBuf>,
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
    /// The account's ignore list: an ignored name is never uploaded, so it is
    /// no local work that makes a folder removed in OneDrive come back.
    pub ignore: IgnoreList,
}

impl Rw {
    /// Read from the store once `staging` holds the new tree: the outbox's
    /// live rows, and what the new tree changes.
    pub fn read(s: &TreeStore, machine: String, upload_differences: bool, ignore: IgnoreList) -> Result<Self, TreeError> {
        let mut rw = Rw { machine, upload_differences, ignore, ..Rw::default() };
        for row in s.outbox_rows()? {
            let Some(id) = row.item_id.clone() else {
                rw.pending.push(row.rel.clone());
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
        self.pending.iter().any(|p| p == rel || is_under(p, rel))
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

/// What became of something OneDrive removed ([`Materializer::remove_in_place`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Removal {
    /// Gone from the disk: it held only what OneDrive had.
    Gone,
    /// Still here, holding nothing the examination would upload — open,
    /// being filled, an ignored name, a symlink, an object from elsewhere:
    /// its removal (and its folder's) waits, attributes and base kept.
    Busy,
    /// Still here, holding local work: its attributes off, uploaded again
    /// as new, and its folder made again in OneDrive.
    Kept,
}

/// Whether an object the examination meets without an item id would be
/// uploaded: a file or a directory whose name is neither ignored, nor the
/// daemon's, nor one OneDrive refuses.
fn uploadable(rw: &Rw, dir: &File, name: &OsStr) -> std::io::Result<bool> {
    if rw.ignore.matches(name) || name.as_encoded_bytes().starts_with(RESERVED_PREFIX.as_bytes()) || names::refused(name).is_some() {
        return Ok(false);
    }
    let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(nix::errno::Errno::ENOENT) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFDIR))
}

/// Where a misplaced entry of the Full scan was, by the base.
enum Was {
    /// At its base place, and the new tree places it elsewhere: moved in
    /// OneDrive — or not the base's at all, as the read phase takes it.
    Moved,
    /// At its base place, and the new tree does not place it: removed in
    /// OneDrive, or no longer placeable.
    Removed,
    /// Away from its base place: a local move or copy not examined yet.
    Elsewhere,
    /// Under the outbox's temporary name in OneDrive (F82 (5)): the local
    /// object stays where it is, and the examination moves the item back.
    Swapped,
    /// An id neither the base nor the new tree has: not ours to remove.
    Stranger,
}

impl Materializer {
    /// The Full scope (§3.7): the scan, then what moved in OneDrive to the
    /// holding directory, then what OneDrive removed, in place, deepest
    /// first; then the new tree top down, as in the read phase.
    pub(super) fn full_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        self.check_cancel()?;
        let scanned = self.disk.scan(&self.root_item_id)?;
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for entry in &scanned {
            if let Some(id) = &entry.id {
                *id_counts.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        // Directories a local change holds, with everything below them. The
        // scan lists a directory's entry before what is in it.
        let mut left_dirs: HashSet<PathBuf> = HashSet::new();
        // What moved in OneDrive goes to the holding directory, what it
        // removed goes in place: deepest first, in one order, so that nothing
        // is moved out from above what is still to be done below it.
        let mut misplaced: Vec<(&Scanned, bool)> = Vec::new();
        for entry in &scanned {
            let Some(id) = &entry.id else { continue };
            // Left where it is, and so is what is below it. The item is left
            // too — not placed, its change waiting — unless another object
            // carries its id: a copy that kept the attributes, which the
            // examination tells from the item.
            let leave = |run: &mut Run, left_dirs: &mut HashSet<PathBuf>| {
                if id_counts.get(id.as_str()).copied().unwrap_or(0) <= 1 {
                    run.left.insert(id.clone());
                    // Its change waits too, whatever the tree does with it:
                    // the examination takes the local move on, and the
                    // outbox meets OneDrive's side (§6).
                    run.out.unsettled.insert(id.clone());
                }
                if entry.is_dir {
                    left_dirs.insert(entry.rel.clone());
                }
            };
            // What a reconcile moved to the holding directory — this one's, or
            // one a stop or a crash cut short — is the daemon's own: the
            // placement takes it from there, or the drain puts it back where
            // it was. Never a local move, never out of the folder.
            if in_holding(&entry.rel) {
                continue;
            }
            // A new folder a stop left under its temporary name goes to the
            // holding directory like anything misplaced: placed from there, or
            // drained. A replacement's leftover link (a file) is the read
            // phase's to recognise, next to its real file.
            if entry.is_dir && is_new_name(&entry.rel) && id_counts.get(id.as_str()).copied().unwrap_or(0) <= 1 {
                misplaced.push((entry, false));
                continue;
            }
            if entry.rel.ancestors().skip(1).any(|a| left_dirs.contains(a)) {
                leave(run, &mut left_dirs);
                continue;
            }
            if is_leftover_replacement(entry, id, &id_counts) {
                self.check_cancel()?;
                self.discard_leftover_replacement(entry, run)?;
                continue;
            }
            if !entry.is_dir && is_new_name(&entry.rel) {
                continue;
            }
            if rw.held.contains(id) || rw.removing.contains(id) {
                leave(run, &mut left_dirs);
                continue;
            }
            if !self.is_misplaced(entry, id)? {
                continue;
            }
            match self.where_it_was(entry, id)? {
                Was::Moved => misplaced.push((entry, false)),
                Was::Removed => misplaced.push((entry, true)),
                Was::Elsewhere => leave(run, &mut left_dirs),
                Was::Swapped | Was::Stranger => {
                    if entry.is_dir {
                        left_dirs.insert(entry.rel.clone());
                    }
                }
            }
        }
        misplaced.sort_by_key(|m| std::cmp::Reverse(m.0.depth));
        for (entry, removed) in misplaced {
            self.check_cancel()?;
            if removed {
                let parent = entry.rel.parent().unwrap_or(Path::new(""));
                let name = entry.rel.file_name().expect("a scanned entry has a name");
                self.remove_in_place(rw, parent, name, run)?;
            } else {
                self.to_holding(&entry.rel, entry.id.as_deref().expect("filtered above"), run)?;
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.lock_dir(&root)?;
        let mut queue = std::collections::VecDeque::from([(self.root_item_id.clone(), PathBuf::new())]);
        while let Some((id, rel)) = queue.pop_front() {
            self.check_cancel()?;
            let children = self.store.with(|s| s.children(Table::Staging, &id))?;
            for row in children {
                if row.placement != Placement::Placed || rw.removing.contains(&row.id) {
                    continue;
                }
                if rw.held.contains(&row.id) || run.left.contains(&row.id) {
                    self.unsettle_tree(&row.id, run)?;
                    continue;
                }
                let Some(placed) = self.place(&row, &rel, run, true)? else {
                    self.unsettle_tree(&row.id, run)?;
                    continue;
                };
                if row.kind == Kind::Folder {
                    queue.push_back((row.id.clone(), placed));
                }
            }
        }
        Ok(())
    }

    fn where_it_was(&self, entry: &Scanned, id: &str) -> Result<Was, ApplyError> {
        let staged = self.store.with(|s| s.get(Table::Staging, id))?;
        if staged.as_ref().is_some_and(|row| row.name.starts_with(SWAP_PREFIX)) {
            return Ok(Was::Swapped);
        }
        let Some(base) = self.store.with(|s| s.get(Table::Items, id))? else {
            // Not the base's: one this very placement left (a cycle stopped
            // before its swap) is put where the tree has it; anything else is
            // a file from elsewhere — another folder, another account — the
            // user's, which the examination takes as new (§3.4 rule 6).
            let placed = self.store.with(|s| s.locate(Table::Staging, id))?.is_some_and(|l| l.placed);
            return Ok(if staged.is_some() && placed { Was::Moved } else { Was::Stranger });
        };
        let at_base = base.placement == Placement::Placed
            && base.parent_id == entry.parent_id
            && entry.rel.file_name() == Some(OsStr::new(&base.name))
            && (base.kind == Kind::Folder) == entry.is_dir;
        if !at_base {
            return Ok(Was::Elsewhere);
        }
        let placed = self.store.with(|s| s.locate(Table::Staging, id))?.is_some_and(|l| l.placed);
        Ok(if staged.is_some() && placed { Was::Moved } else { Was::Removed })
    }

    /// The Changed scope (§3.7): as the read phase's, but a disagreement a
    /// local change explains never turns it Full — an item not where the base
    /// has it is left to the examination, and its change waits. Only
    /// something to place again below a folder that is not where the tree
    /// says asks for the scan.
    pub(super) fn changed_rw(&self, rw: &Rw, ids: Vec<String>, run: &mut Run) -> Result<(), ApplyError> {
        if self.holding_if_any()?.is_some() {
            return Err(ApplyError::NeedFull(format!("{HOLDING} is left from an earlier run")));
        }
        let mut scope: HashSet<String> = ids.iter().cloned().collect();
        scope.remove(&self.root_item_id);
        for id in &ids {
            let new = self.store.with(|s| s.locate(Table::Staging, id))?;
            let old = self.store.with(|s| s.locate(Table::Items, id))?;
            if new.as_ref().is_some_and(|l| l.placed) && !old.as_ref().is_some_and(|l| l.placed) {
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
        here.sort_by_key(|h| std::cmp::Reverse(h.1.depth));
        for (id, old) in &here {
            self.check_cancel()?;
            if rw.removing.contains(id) {
                continue;
            }
            if rw.held.contains(id) {
                run.out.unsettled.insert(id.clone());
                continue;
            }
            let parent = old.rel.parent().unwrap_or(Path::new(""));
            let Some(name) = old.rel.file_name() else { continue };
            let found = match self.disk.dir(parent) {
                Ok(dir) => self.disk.probe(&dir, name)?,
                Err(_) => Probe::Absent,
            };
            if !matches!(&found, Probe::Managed { id: found, .. } if found == id) {
                // Deleted or moved here, not examined yet. Where the tree
                // still places it, phase 2 decides; where it removes it, the
                // removal waits: the examination takes the local change
                // on, and the outbox meets OneDrive's side (§6).
                run.missing.insert(id.clone());
                if !self.store.with(|s| s.locate(Table::Staging, id))?.is_some_and(|l| l.placed) {
                    run.out.unsettled.insert(id.clone());
                }
                continue;
            }
            let old_row = self.store.with(|s| s.get(Table::Items, id))?;
            let new_row = self.store.with(|s| s.get(Table::Staging, id))?;
            let stays = matches!((&old_row, &new_row), (Some(o), Some(n))
                if n.placement == Placement::Placed && n.parent_id == o.parent_id && n.name == o.name);
            if stays {
                continue;
            }
            if self.store.with(|s| s.locate(Table::Staging, id))?.is_some_and(|l| l.placed) {
                self.to_holding(&old.rel, id, run)?;
            } else {
                self.remove_in_place(rw, parent, name, run)?;
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
        there.sort_by_key(|t| t.1.depth);
        let mut placed: HashSet<String> = HashSet::new();
        for (row, new) in &there {
            self.check_cancel()?;
            if rw.removing.contains(&row.id) {
                continue;
            }
            if rw.held.contains(&row.id) || run.out.unsettled.contains(&row.id) {
                self.unsettle_tree(&row.id, run)?;
                continue;
            }
            let parent = new.rel.parent().unwrap_or(Path::new(""));
            if let Err(e) = self.check_parent(row, parent, &placed) {
                // Its folder is not where the tree has it. With nothing to
                // place, the change waits; with something, only a scan can
                // tell a folder deleted here from one moved (§3.7).
                if rw.revive.contains(&row.id) {
                    return Err(e);
                }
                self.unsettle_tree(&row.id, run)?;
                continue;
            }
            match self.place(row, parent, run, false)? {
                Some(_) if row.kind == Kind::Folder => {
                    placed.insert(row.id.clone());
                }
                Some(_) => {}
                None => self.unsettle_tree(&row.id, run)?,
            }
        }
        Ok(())
    }

    /// Whether another item of ours at `rel` is there by a local change —
    /// with a row, below one, or away from where the base has it — so that
    /// it keeps the name: the outbox settles the two (§6).
    pub(super) fn holds_the_name(&self, rw: &Rw, other: &str, rel: &Path, run: &Run) -> Result<bool, ApplyError> {
        if rw.held.contains(other) || rw.removing.contains(other) || run.left.contains(other) {
            return Ok(true);
        }
        let base = self.store.with(|s| s.locate(Table::Items, other))?;
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
    pub(super) fn place_again(&self, rw: &Rw, row: &crate::tree::Row, rel: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        if !rw.revive.contains(&row.id) {
            return Ok(false);
        }
        if self.store.with(|s| s.local_handle(&row.id))?.is_some() {
            let base = self.store.with(|s| s.locate(Table::Items, &row.id))?.filter(|l| l.placed).map(|l| l.rel);
            run.out.examine.push((base.unwrap_or_else(|| rel.to_path_buf()), false));
            return Ok(false);
        }
        Ok(true)
    }

    /// `id` and everything the base or the new tree has below it wait.
    fn unsettle_tree(&self, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        run.out.unsettled.insert(id.to_owned());
        let below = self.store.with(|s| {
            let mut ids = s.descendants(Table::Staging, id)?;
            ids.extend(s.descendants(Table::Items, id)?);
            Ok(ids)
        })?;
        run.out.unsettled.extend(below);
        Ok(())
    }

    /// What OneDrive removed, at `parent/name`: whatever holds only what the
    /// cloud had goes; a file with local work stays, its konedrive
    /// attributes off (the examination uploads it again, §6 edit × delete);
    /// a folder that keeps local work stays too, made local, and is made
    /// again in OneDrive (F82 (4)). What cannot go now and is no local work —
    /// a file open somewhere or being filled, an ignored name, a symlink, an
    /// object from elsewhere — keeps its attributes and its base, and so does
    /// every folder above it: their removal waits.
    pub(super) fn remove_in_place(&self, rw: &Rw, parent: &Path, name: &OsStr, run: &mut Run) -> Result<Removal, ApplyError> {
        let rel = parent.join(name);
        let dir = self.disk.dir(parent)?;
        match self.disk.probe(&dir, name)? {
            Probe::Absent => Ok(Removal::Gone),
            Probe::Unmanaged { is_dir } => {
                if uploadable(rw, &dir, name)? {
                    run.out.examine.push((rel, is_dir));
                    Ok(Removal::Kept)
                } else {
                    Ok(Removal::Busy)
                }
            }
            Probe::Managed { id, is_dir } => {
                // A local change the outbox or the examination takes on.
                if rw.held.contains(&id) || run.left.contains(&id) {
                    return Ok(Removal::Kept);
                }
                if rw.removing.contains(&id) {
                    return Ok(Removal::Busy);
                }
                // An id the base does not have: a file from elsewhere — another
                // folder, another account whose outbox may still have to fetch
                // it (§4.5) — never ours to remove; the examination decides.
                if self.store.with(|s| s.get(Table::Items, &id))?.is_none() {
                    run.out.examine.push((rel, is_dir));
                    return Ok(Removal::Busy);
                }
                // Moved here, and still placed elsewhere by the tree: its own
                // change, not this folder's.
                if self.store.with(|s| s.locate(Table::Staging, &id))?.is_some_and(|l| l.placed && l.rel != rel) {
                    run.out.unsettled.insert(id);
                    return Ok(Removal::Busy);
                }
                if !is_dir {
                    let file = self.disk.open_file(&dir, name)?;
                    return self.remove_file_in_place(rw, &dir, name, &rel, &id, file, run);
                }
                let sub = self.disk.open_subdir(&dir, name)?;
                let mut outcome = Removal::Gone;
                for child in self.disk.list(&sub)? {
                    self.check_cancel()?;
                    outcome = outcome.max(self.remove_in_place(rw, &rel, &child, run)?);
                }
                match outcome {
                    Removal::Gone => {
                        self.disk.remove(&dir, name, true)?;
                        run.out.deleted += 1;
                        run.note(EventKind::Removed, &rel, None);
                    }
                    Removal::Busy => {
                        tracing::info!("{} was removed from OneDrive; it goes once nothing in it is in use", rel.display());
                        run.out.unsettled.insert(id);
                    }
                    Removal::Kept => {
                        placeholder::strip_konedrive_xattrs(&sub)?;
                        tracing::info!("{} was removed from OneDrive but holds local work: it stays, and is made again there", rel.display());
                        run.out.recreated.push(id);
                        run.out.examine.push((rel, true));
                    }
                }
                Ok(outcome)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn remove_file_in_place(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, id: &str, file: File, run: &mut Run) -> Result<Removal, ApplyError> {
        let keep = |run: &mut Run| -> Result<Removal, ApplyError> {
            placeholder::strip_konedrive_xattrs(&file)?;
            tracing::info!("{} was removed from OneDrive but holds local work: it stays, and is uploaded again", rel.display());
            run.out.examine.push((rel.to_path_buf(), false));
            Ok(Removal::Kept)
        };
        match read_state(&file) {
            Ok(Some(State::Hydrating | State::Dehydrating)) => {
                run.out.deferred += 1;
                run.out.unsettled.insert(id.to_owned());
                Ok(Removal::Busy)
            }
            Ok(Some(State::Hydrated)) => {
                // Nobody writes into it as it goes: a writer that opened it
                // would lose what it writes into an unlinked inode (§3.7). The
                // lease first, then the look at its content.
                let Some(_lease) = WriteLease::take(&file)? else {
                    run.out.unsettled.insert(id.to_owned());
                    return Ok(Removal::Busy);
                };
                if self.local_work(&file) || rw.upload_differences {
                    return keep(run);
                }
                self.disk.remove(dir, name, false)?;
                run.out.deleted += 1;
                run.note(EventKind::Removed, rel, None);
                Ok(Removal::Gone)
            }
            _ if self.local_work(&file) => keep(run),
            _ => {
                if rw.upload_differences {
                    tracing::info!("{} is not in OneDrive's new listing and held nothing here: removed", rel.display());
                }
                self.disk.remove(dir, name, false)?;
                run.out.deleted += 1;
                run.note(EventKind::Removed, rel, None);
                Ok(Removal::Gone)
            }
        }
    }

    /// Keeps both (§6): the object at `dir/name` (at `rel`) is renamed in its
    /// directory to the first free `name-<machine>.ext`, never over anything,
    /// and becomes the user's own — konedrive's attributes off — to be
    /// uploaded as new; the name is the cloud's again. Rows below a directory
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
            placeholder::strip_konedrive_xattrs(&self.disk.open_subdir(dir, copied)?)?;
        } else if let Ok(file) = self.disk.open_file(dir, copied) {
            placeholder::strip_konedrive_xattrs(&file)?;
        }
        let copy_rel = rel.with_file_name(&copy);
        if is_dir {
            self.store.with(|s| s.outbox_apply(&[OutboxOp::Rebase { from: rel.to_path_buf(), to: copy_rel.clone() }], 0))?;
        }
        tracing::info!("{} changed here and in OneDrive: the local version is kept as {}", rel.display(), copy_rel.display());
        run.out.copies.push(Copied { original: rel.to_path_buf(), copy: copy_rel.clone() });
        run.out.examine.push((copy_rel, is_dir));
        Ok(())
    }

    /// What is left in the holding directory — what this run moved there and
    /// could not place (a local change holds its new folder), or what a stop
    /// or a crash left: what OneDrive removed goes as
    /// [`Self::remove_in_place`] says; the rest goes back into the folder
    /// ([`Self::put_back`]). Nothing leaves the folder.
    pub(super) fn drain_holding_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let id = name.to_str().map(str::to_owned);
            let placed = match &id {
                Some(id) => self.store.with(|s| s.locate(Table::Staging, id))?.is_some_and(|l| l.placed),
                None => false,
            };
            if !placed && self.finish_new_folder(&holding, &name, id.as_deref(), run)? {
                continue;
            }
            if !placed && self.remove_in_place(rw, Path::new(HOLDING), &name, run)? == Removal::Gone {
                continue;
            }
            if let Some(id) = &id {
                if placed {
                    run.out.unsettled.insert(id.clone());
                }
            }
            let back = id.as_deref().and_then(|id| run.moved_from.get(id).cloned());
            self.put_back(&holding, &name, id.as_deref(), back, run)?;
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.remove(&root, OsStr::new(HOLDING), true)?;
        Ok(())
    }

    /// A new folder a stop left under its temporary name (`.konedrive-new-<id>`)
    /// whose item neither the base nor the tree has any more — OneDrive removed
    /// it meanwhile: it was only ever the daemon's, so it goes, and never comes
    /// back under its id as a name. Anything someone put in it is
    /// put back where the folder stood, each under its own name. Whether it was
    /// such a folder.
    fn finish_new_folder(&self, holding: &File, name: &OsStr, id: Option<&str>, run: &mut Run) -> Result<bool, ApplyError> {
        let Some(id) = id else { return Ok(false) };
        if !matches!(self.disk.probe(holding, name)?, Probe::Managed { is_dir: true, .. }) {
            return Ok(false);
        }
        let from = run.moved_from.get(id).cloned();
        if from.as_ref().is_some_and(|f| !is_new_name(f)) || self.store.with(|s| Ok(s.get(Table::Items, id)?.is_some() || s.get(Table::Staging, id)?.is_some()))? {
            return Ok(false);
        }
        let parent = from.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf).unwrap_or_default();
        let sub = self.disk.open_subdir(holding, name)?;
        for child in self.disk.list(&sub)? {
            if !self.put_at(&sub, &child, &parent.join(&child), run)? {
                self.put_at(&sub, &child, Path::new(&child), run)?;
            }
        }
        self.disk.remove(holding, name, true)?;
        Ok(true)
    }

    /// Takes `name` out of the holding directory, back into the folder: to
    /// `back`, where this run took it from; else where the base has item
    /// `id`; else where the tree has it; each beside its name under a copy
    /// name when that is taken now; and last, under a copy name in the root.
    /// Never out of the folder: whatever is out of it is a move out, and
    /// deleted in OneDrive.
    fn put_back(&self, holding: &File, name: &OsStr, id: Option<&str>, back: Option<PathBuf>, run: &mut Run) -> Result<(), ApplyError> {
        let mut places: Vec<PathBuf> = back.into_iter().filter(|b| !is_new_name(b)).collect();
        if let Some(id) = id {
            for table in [Table::Items, Table::Staging] {
                if let Some(at) = self.store.with(|s| s.locate(table, id))?.filter(|l| l.placed && !l.rel.as_os_str().is_empty()) {
                    places.push(at.rel);
                }
            }
        }
        let fallback = places.first().and_then(|p| p.file_name()).map(|n| n.to_owned()).unwrap_or_else(|| name.to_owned());
        places.push(PathBuf::from(fallback));
        for place in &places {
            if self.put_at(holding, name, place, run)? {
                return Ok(());
            }
        }
        Err(ApplyError::Io(format!("{} could not be put back into the folder", PathBuf::from(HOLDING).join(name).display())))
    }

    /// Renames `name` from the holding directory to `place`, or beside it
    /// under the first free copy name. Whether it went: `false` when
    /// `place`'s folder is not there.
    fn put_at(&self, holding: &File, name: &OsStr, place: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        let parent = place.parent().unwrap_or(Path::new(""));
        let Some(to) = place.file_name() else { return Ok(false) };
        let Ok(dir) = self.disk.dir(parent) else { return Ok(false) };
        let machine = self.rw.as_ref().map(|rw| rw.machine.clone()).unwrap_or_default();
        let mut candidates = vec![to.to_os_string()];
        if let Some(wanted) = to.to_str() {
            candidates.extend((1..=100).map(|n| copy_name(wanted, &machine, n).into()));
        }
        for candidate in candidates {
            match self.disk.rename(holding, name, &dir, &candidate) {
                Ok(()) => {
                    let is_dir = matches!(self.disk.probe(&dir, &candidate)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
                    run.out.examine.push((parent.join(&candidate), is_dir));
                    return Ok(true);
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
