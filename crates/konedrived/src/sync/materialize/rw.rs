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
//! - **removes what OneDrive removed, whole, in the cycle** (issue #104):
//!   whatever is there — a changed file, a new one, a file open in a
//!   program, an ignored name, a symlink, an object from elsewhere — goes,
//!   a download into it stopped; nothing is rescued, uploaded again or made
//!   again in OneDrive (`resyncChangesUploadDifferences` alone keeps
//!   downloads and local work, §3.7);
//! - **lets what is no longer placed leave once its uploads are done**
//!   (issue #104): its placement is the base's at once, its object stays
//!   and is examined, and goes once nothing in it waits for the outbox
//!   ([`Materializer::leaving_rw`]);
//! - **forgets before it removes**: the local objects of everything it takes
//!   off the disk are forgotten in the store first, so that no examination
//!   can prove one gone and delete it in OneDrive;
//! - **never moves anything out of the folder**: what it moved to the holding
//!   directory — this run, or one a stop cut short — is placed from there or
//!   put back where it was, never rescued outside, where it would be taken
//!   for a move out and deleted in OneDrive.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, read_state, State};

use std::os::fd::AsFd;

use super::{is_leftover_replacement, ApplyError, Copied, Materializer, Run};
use konedrive_graph::drive::item::RESERVED_PREFIX;
use crate::sync::activity::Kind as EventKind;
use crate::sync::disk::{Probe, Scanned, HOLDING, NEW_PREFIX};
use crate::sync::local::{names, IgnoreList};
use crate::sync::upload::copy_name;
use konedrive_tree::outbox::{OutboxOp, SWAP_PREFIX};
use konedrive_tree::{Kind, Placement, Table, TreeError, TreeStore};

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
    /// The account's ignore list: an ignored name is never uploaded, so it
    /// keeps no folder that stopped being placed waiting on disk.
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
    /// Gone from the disk, whole.
    Gone,
    /// `resyncChangesUploadDifferences` only (§3.7): a download or local
    /// work the new listing left out stays, its attributes off, uploaded
    /// again as new — and its folder with it, made again in OneDrive.
    Kept,
}

/// How long one removal waits in all for the downloads it stopped to let go
/// of their files (a guess; the cycle waits meanwhile).
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What is on disk at and below something about to be taken off it: what
/// the daemon forgets first, and the fills it stops (issue #104).
#[derive(Default)]
pub(super) struct Survey {
    /// Item ids the objects carry.
    ids: Vec<String>,
    /// The objects themselves.
    handles: Vec<konedrive_fs::handle::FileHandle>,
    /// The files, for their fills.
    files: Vec<crate::sync::InodeKey>,
    /// The files whose fill was told to stop.
    stopped: Vec<crate::sync::InodeKey>,
}

impl Survey {
    /// A survey that knows only which fills were stopped.
    pub(super) fn stopped_only(stopped: Vec<crate::sync::InodeKey>) -> Self {
        Survey { stopped, ..Survey::default() }
    }

    pub(super) fn stopped_keys(&self) -> &[crate::sync::InodeKey] {
        &self.stopped
    }
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
    /// At its base place, or where it stays while it leaves, and the new
    /// tree does not have it: removed in OneDrive.
    Removed,
    /// At its base place, and the new tree has it but does not place it: no
    /// longer placeable here (issue #104).
    Unplaced,
    /// Where it stays while it leaves ([`Materializer::leaving_rw`]).
    Leaving,
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
        // Directories no longer placed, which stay on disk for now with
        // everything in them, as it is (issue #104).
        let mut leaving_dirs: HashSet<PathBuf> = HashSet::new();
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
            if entry.rel.ancestors().skip(1).any(|a| leaving_dirs.contains(a)) {
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
                // Whatever a local change holds, what OneDrive removed goes,
                // and what is no longer placed is the base's (issue #104).
                let was = if self.is_misplaced(entry, id)? { Some(self.where_it_was(entry, id)?) } else { None };
                match was {
                    Some(Was::Removed) => misplaced.push((entry, true)),
                    Some(Was::Unplaced) => {
                        self.unplace(&entry.rel, id, entry.is_dir, run)?;
                        leaving_dirs.insert(entry.rel.clone());
                    }
                    Some(Was::Leaving) => {
                        leaving_dirs.insert(entry.rel.clone());
                    }
                    _ => leave(run, &mut left_dirs),
                }
                continue;
            }
            if !self.is_misplaced(entry, id)? {
                continue;
            }
            match self.where_it_was(entry, id)? {
                Was::Moved => misplaced.push((entry, false)),
                Was::Removed => misplaced.push((entry, true)),
                Was::Unplaced => {
                    self.unplace(&entry.rel, id, entry.is_dir, run)?;
                    leaving_dirs.insert(entry.rel.clone());
                }
                // What is below it stays with it, as it is: the leaving pass
                // decides.
                Was::Leaving => {
                    leaving_dirs.insert(entry.rel.clone());
                }
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
            let children = self.store.call_blocking(move |s| s.children(Table::Staging, &id))?;
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
        let staged = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?;
        if staged.as_ref().is_some_and(|row| row.name.starts_with(SWAP_PREFIX)) {
            return Ok(Was::Swapped);
        }
        let Some(base) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Items, &id) })? else {
            // Not the base's: one this very placement left (a cycle stopped
            // before its swap) is put where the tree has it; anything else is
            // a file from elsewhere — another folder, another account — the
            // user's, which the examination takes as new (§3.4 rule 6).
            let placed = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed);
            return Ok(if staged.is_some() && placed { Was::Moved } else { Was::Stranger });
        };
        let placed = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed);
        let base_placed = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Items, &id) })?.is_some_and(|l| l.placed);
        // An object that stays while it leaves (issue #104), whether or not
        // its item is placed again elsewhere since.
        // Recognised by its item id wherever it is — a parent renamed in
        // OneDrive or here took it along — unless it is the object the new
        // tree places right there; its place is followed.
        // Only the object itself: at its recorded place, or carrying its
        // recorded file handle — never another object with its id (the copy
        // placed again, a copy, a hard link).
        let leaving = self.store.call_blocking(|s| s.leaving_with_handles())?.into_iter().find(|(left, _, _)| left == id);
        // Where a handle is kept, the recorded place counts only for the
        // object carrying it; elsewhere, only an object with one link (a hard
        // link carries the same handle, and is the user's name).
        // At its recorded place, an object with its id is it — after an
        // editor's save by rename too, its handle then taken anew — unless the
        // item is placed elsewhere, where the copy placed again may stand
        // here by the user's move: then only its handle tells.
        let elsewhere = self.store.call_blocking({ let (id, rel) = (id.to_owned(), entry.rel.clone()); move |s| s.placed_elsewhere(&id, &rel) })?;
        let mut renewed = None;
        let itself = leaving.as_ref().is_some_and(|(_, at, handle)| match handle {
            None => *at == entry.rel,
            Some(h) => {
                let (parent, name) = (entry.rel.parent().unwrap_or(Path::new("")), entry.rel.file_name());
                let dir = self.disk.dir(parent).ok();
                let here = name.zip(dir.as_ref()).and_then(|(name, dir)| konedrive_fs::handle::FileHandle::at(dir, name).ok());
                let same = here.as_ref() == Some(h);
                let single = name.zip(dir.as_ref()).is_some_and(|(name, dir)| {
                    nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|s| entry.is_dir || s.st_nlink <= 1)
                });
                if same {
                    *at == entry.rel || single
                } else if *at == entry.rel && !elsewhere {
                    renewed = here;
                    true
                } else {
                    false
                }
            }
        });
        if let (true, Some(handle)) = (itself, renewed) {
            self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_set_handle(&id, &handle) })?;
        }
        let leaving = leaving.map(|(_, at, _)| at).filter(|_| itself);
        let placed_here = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed && l.rel == entry.rel);
        if let (Some(at), Some(_), false) = (&leaving, &staged, placed_here) {
            if *at != entry.rel {
                self.store.call_blocking({ let (id, rel) = (id.to_owned(), entry.rel.clone()); move |s| s.leaving_set_rel(&id, &rel) })?;
            }
            return Ok(Was::Leaving);
        }
        if !base_placed {
            // Not placed by the base. Where the base has it, it is no longer
            // placeable; anywhere else it is the user's move out of what is
            // leaving, carried out as any other (issue #104).
            let at_base_place = base.parent_id == entry.parent_id && entry.rel.file_name() == Some(OsStr::new(&base.name)) && (base.kind == Kind::Folder) == entry.is_dir;
            return Ok(match (staged.is_some(), placed) {
                (false, _) => Was::Removed,
                (true, false) if at_base_place => Was::Unplaced,
                (true, false) => Was::Elsewhere,
                // Placed by the tree, and not by the base: as for anything
                // away from its base place, the examination decides first.
                (true, true) => Was::Elsewhere,
            });
        }
        let at_base = base.placement == Placement::Placed
            && base.parent_id == entry.parent_id
            && entry.rel.file_name() == Some(OsStr::new(&base.name))
            && (base.kind == Kind::Folder) == entry.is_dir;
        if !at_base {
            return Ok(Was::Elsewhere);
        }
        Ok(match (staged.is_some(), placed) {
            (true, true) => Was::Moved,
            (true, false) => Was::Unplaced,
            (false, _) => Was::Removed,
        })
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
            let new = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?;
            let old = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Items, &id) })?;
            if new.as_ref().is_some_and(|l| l.placed) && !old.as_ref().is_some_and(|l| l.placed) {
                scope.extend(self.store.call_blocking({ let id = id.to_owned(); move |s| s.descendants(Table::Staging, &id) })?);
            }
        }
        run.scope = Some(scope.clone());

        // Phase 1, by where things are now, deepest first.
        let mut here = Vec::new();
        for id in &scope {
            if let Some(old) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Items, &id) })?.filter(|l| l.placed) {
                here.push((id.clone(), old));
            }
        }
        here.sort_by_key(|h| std::cmp::Reverse(h.1.depth));
        for (id, old) in &here {
            self.check_cancel()?;
            if rw.removing.contains(id) {
                continue;
            }
            let staged = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?;
            let placed_now = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed);
            let parent = old.rel.parent().unwrap_or(Path::new(""));
            let Some(name) = old.rel.file_name() else { continue };
            let found = match self.disk.dir(parent) {
                Ok(dir) => self.disk.probe(&dir, name)?,
                Err(_) => Probe::Absent,
            };
            let at_place = matches!(&found, Probe::Managed { id: found, .. } if found == id);
            // A local change holds it — unless OneDrive removed it, or it is
            // no longer placed, and its object is where the base has it: that
            // goes, or is the base's, whatever holds it (issue #104).
            if rw.held.contains(id) && (placed_now || !at_place) {
                run.out.unsettled.insert(id.clone());
                continue;
            }
            if !at_place {
                // Deleted or moved here, not examined yet. Where the tree
                // still places it, phase 2 decides; where it removes it, the
                // removal waits: the examination takes the local change
                // on, and the outbox meets OneDrive's side (§6).
                run.missing.insert(id.clone());
                if !self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed) {
                    run.out.unsettled.insert(id.clone());
                }
                continue;
            }
            let old_row = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Items, &id) })?;
            let stays = matches!((&old_row, &staged), (Some(o), Some(n))
                if n.placement == Placement::Placed && n.parent_id == o.parent_id && n.name == o.name);
            if stays {
                continue;
            }
            match (&staged, placed_now) {
                (Some(_), true) => self.to_holding(&old.rel, id, run)?,
                (Some(_), false) => self.unplace(&old.rel, id, matches!(found, Probe::Managed { is_dir: true, .. }), run)?,
                (None, _) => {
                    self.remove_in_place(rw, parent, name, run)?;
                }
            }
        }

        // Phase 2, by where things belong, shallowest first.
        let mut there = Vec::new();
        for id in &scope {
            let row = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?;
            let new = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?;
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
            run.out.examine.push((base.unwrap_or_else(|| rel.to_path_buf()), false));
            return Ok(false);
        }
        Ok(true)
    }

    /// `id` and everything the base or the new tree has below it wait.
    fn unsettle_tree(&self, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        run.out.unsettled.insert(id.to_owned());
        let folder = id.to_owned();
        let below = self.store.call_blocking(move |s| {
            let mut ids = s.descendants(Table::Staging, &folder)?;
            ids.extend(s.descendants(Table::Items, &folder)?);
            Ok(ids)
        })?;
        run.out.unsettled.extend(below);
        Ok(())
    }

    /// What OneDrive removed, at `parent/name`, goes from the disk whole,
    /// in this cycle (issue #104): copies of OneDrive's content, files
    /// changed here, new files, files open in a program, ignored names,
    /// symlinks, objects from elsewhere — nothing is rescued, uploaded again
    /// or made again in OneDrive. Before anything goes, every object there
    /// is forgotten in the store ([`TreeStore::forget_local_objects`]), and
    /// a download into one of them stops. The rows that would still upload
    /// or move something there go too. Only `resyncChangesUploadDifferences`
    /// keeps what was downloaded or changed here (§3.7). An object that will
    /// not go fails the cycle.
    ///
    /// [`TreeStore::forget_local_objects`]: konedrive_tree::TreeStore::forget_local_objects
    pub(super) fn remove_in_place(&self, rw: &Rw, parent: &Path, name: &OsStr, run: &mut Run) -> Result<Removal, ApplyError> {
        let rel = parent.join(name);
        let dir = self.disk.dir(parent)?;
        if matches!(self.disk.probe(&dir, name)?, Probe::Absent) {
            return Ok(Removal::Gone);
        }
        let survey = self.forget_before_removing(&dir, name, true)?;
        run.out.taken.extend(survey.ids.iter().cloned());
        let outcome = self.remove_whole(rw.upload_differences.then_some(rw), &dir, name, &rel, run);
        if outcome.is_err() {
            self.settle_stopped(&dir, name, &survey);
        }
        let outcome = outcome?;
        let dropped = self.store.call_blocking({ let rel = rel.clone(); move |s| s.outbox_drop_under(&rel) })?;
        if !dropped.is_empty() {
            tracing::info!("{} was removed from OneDrive: {} change(s) waiting there are dropped", rel.display(), dropped.len());
        }
        Ok(outcome)
    }

    /// Decision 5 of issue #104: before anything at `dir/name` is taken off
    /// the disk, the store forgets every local object there — and, `by_id`,
    /// the local objects of every item whose id an object there carries, and
    /// of everything the tree has below it — and fills into its files stop.
    /// What was found.
    pub(super) fn forget_before_removing(&self, dir: &File, name: &OsStr, by_id: bool) -> Result<Survey, ApplyError> {
        let mut survey = Survey::default();
        let dev = nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev;
        self.survey(dir, name, dev, &mut survey)?;
        let ids = if by_id { survey.ids.clone() } else { Vec::new() };
        let handles = survey.handles.clone();
        self.store.call_blocking(move |s| s.forget_local_objects(&ids, &handles))?;
        for key in survey.files.clone() {
            if self.locks.cancel(key) {
                tracing::info!("a download into a file being removed is stopped");
                survey.stopped.push(key);
            }
        }
        Ok(survey)
    }

    fn survey(&self, dir: &File, name: &OsStr, dev: libc::dev_t, survey: &mut Survey) -> Result<(), ApplyError> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        if let Ok(handle) = konedrive_fs::handle::FileHandle::at(dir, name) {
            survey.handles.push(handle);
        }
        let probed = self.disk.probe(dir, name)?;
        if let Probe::Managed { id, .. } = &probed {
            survey.ids.push(id.clone());
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => survey.files.push(crate::sync::InodeKey { dev: stat.st_dev as u64, ino: stat.st_ino as u64 }),
            // Never into another filesystem mounted here: removing the
            // directory it is mounted on fails the cycle instead.
            libc::S_IFDIR if stat.st_dev == dev => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.survey(&sub, &child, dev, survey)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Takes `dir/name` (at `rel`) off the disk with everything below it.
    /// `keep` is the plan of a `resyncChangesUploadDifferences` listing,
    /// which keeps downloads and local work (§3.7).
    fn remove_whole(&self, keep: Option<&Rw>, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<Removal, ApplyError> {
        self.check_cancel()?;
        let probed = self.disk.probe(dir, name)?;
        let id = match &probed {
            Probe::Absent => return Ok(Removal::Gone),
            Probe::Managed { id, .. } => Some(id.clone()),
            Probe::Unmanaged { .. } => None,
        };
        let is_dir = matches!(probed, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        if is_dir {
            let sub = self.disk.open_subdir(dir, name)?;
            let mut outcome = Removal::Gone;
            if nix::sys::stat::fstat(sub.as_fd()).map_err(std::io::Error::from)?.st_dev == nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev {
                for child in self.disk.list(&sub)? {
                    outcome = outcome.max(self.remove_whole(keep, &sub, &child, &rel.join(&child), run)?);
                }
            }
            if outcome == Removal::Kept {
                if let Some(id) = id {
                    placeholder::strip_konedrive_xattrs(&sub)?;
                    tracing::info!("{} is not in OneDrive's new listing but holds local work: it stays, and is made again there", rel.display());
                    run.out.recreated.push(id);
                }
                run.out.examine.push((rel.to_path_buf(), true));
                return Ok(Removal::Kept);
            }
            self.disk.remove(dir, name, true)?;
        } else {
            if let Some(rw) = keep {
                if self.kept_by_resync(rw, dir, name, id.is_some())? {
                    run.out.examine.push((rel.to_path_buf(), false));
                    return Ok(Removal::Kept);
                }
            }
            if id.is_some() {
                self.release_other_names(dir, name, rel)?;
            }
            self.disk.remove(dir, name, false)?;
        }
        if id.is_some() {
            run.out.deleted += 1;
            run.note(EventKind::Removed, rel, None);
        }
        Ok(Removal::Gone)
    }

    /// A file of ours about to be taken off the disk that has other names —
    /// hard links the user made — keeps them, but not as the item: its item
    /// id goes from the inode first, so that what stays is the user's own
    /// file, never the item under another name (issue #104, decision 1). A
    /// downloaded one goes up as new; one not downloaded waits as not
    /// downloaded, its state kept, so that it is never read as zeros.
    fn release_other_names(&self, dir: &File, name: &OsStr, rel: &Path) -> Result<(), ApplyError> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        if stat.st_nlink <= 1 {
            return Ok(());
        }
        let file = self.disk.open_file(dir, name)?;
        match xattr::FileExt::remove_xattr(&file, placeholder::XATTR_ITEM_ID) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {}
            Err(e) => return Err(e.into()),
        }
        tracing::info!("{} has other names, which stay as the user's own files", rel.display());
        Ok(())
    }

    /// `resyncChangesUploadDifferences` (§3.7): a download, a file with local
    /// work, or a new file the new listing left out stays, to be uploaded
    /// again as new — its konedrive attributes off.
    fn kept_by_resync(&self, rw: &Rw, dir: &File, name: &OsStr, managed: bool) -> Result<bool, ApplyError> {
        if !managed {
            return Ok(uploadable(rw, dir, name)?);
        }
        let file = self.disk.open_file(dir, name)?;
        if !(matches!(read_state(&file), Ok(Some(State::Hydrated))) || self.local_work(&file)) {
            return Ok(false);
        }
        placeholder::strip_konedrive_xattrs(&file)?;
        tracing::info!("{} is not in OneDrive's new listing and was downloaded here: it stays, and is uploaded again", name.to_string_lossy());
        Ok(true)
    }

    /// Item `id`, at `rel`, stays in OneDrive but is no longer placed here —
    /// a name too long, a reserved one, the Personal Vault, shared, OneNote
    /// (issue #104, decision 3). Its placement is the base's in this cycle
    /// whatever holds it, and its local objects are forgotten; its object
    /// stays where it is for now, and is examined, so that what waits to be
    /// uploaded from inside it gets its row and goes up into the item in
    /// OneDrive. [`Self::leaving_rw`] removes it once nothing does.
    fn unplace(&self, rel: &Path, id: &str, is_dir: bool, run: &mut Run) -> Result<(), ApplyError> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        let dir = self.disk.dir(parent)?;
        let handles: Vec<_> = konedrive_fs::handle::FileHandle::at(&dir, name).ok().into_iter().collect();
        let (ids, at) = (vec![id.to_owned()], rel.to_path_buf());
        self.store.call_blocking(move |s| {
            s.forget_local_objects(&ids, &handles)?;
            s.leaving_add(&ids[0], &at, handles.first())
        })?;
        tracing::info!("{} is no longer placed here; it goes once nothing in it waits to be uploaded", rel.display());
        run.out.examine.push((rel.to_path_buf(), is_dir));
        run.out.taken.insert(id.to_owned());
        run.unplaced.insert(id.to_owned());
        Ok(())
    }

    /// What stopped being placed and stays on disk for now ([`Self::unplace`]):
    /// placed again where it is, it is the item's again; removed in OneDrive
    /// since, it goes as [`Self::remove_in_place`] says; otherwise it goes
    /// whole once no outbox row has a place at or below it and nothing in it
    /// waits to be examined and uploaded (a new file, a changed download).
    /// Until then it is examined again.
    pub(super) fn leaving_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        let leaving = self.store.call_blocking(|s| s.leaving())?;
        for (id, rel) in leaving {
            self.check_cancel()?;
            if run.unplaced.contains(&id) {
                // Examined first: a later cycle decides.
                continue;
            }
            // At its path; else, gone from there (`ENOENT`) or another object
            // there, wherever its file handle finds it — a parent renamed here
            // and not examined yet — its path followed. Its row goes only when
            // neither finds it. Any other error decides nothing, and the row
            // stays.
            let found = match self.leaving_at(&rel, &id) {
                Ok(found) => found,
                Err(e) => {
                    tracing::warn!("{} is leaving and cannot be looked at ({e}); it is looked at again later", rel.display());
                    continue;
                }
            };
            let found = match found {
                Some(found) => Some((rel.clone(), found)),
                None => match self.leaving_by_handle(&id) {
                    Ok(Some(at)) => match self.leaving_at(&at, &id) {
                        Ok(Some(found)) => {
                            tracing::info!("{} is leaving and was found at {} by its handle", rel.display(), at.display());
                            self.store.call_blocking({ let (id, at) = (id.clone(), at.clone()); move |s| s.leaving_set_rel(&id, &at) })?;
                            Some((at, found))
                        }
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!("{} is leaving and cannot be looked at ({e}); it is looked at again later", at.display());
                            continue;
                        }
                    },
                    Ok(None) => None,
                    Err(ApplyError::Cancelled) => return Err(ApplyError::Cancelled),
                    Err(e) => {
                        tracing::warn!("{} is leaving and the folder cannot be searched for it ({e}); it is looked at again later", rel.display());
                        continue;
                    }
                },
            };
            let Some((rel, (is_dir, dir))) = found else {
                // Gone, or not its object any more.
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            };
            let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
            let name = rel.file_name().map(OsStr::to_os_string).expect("a found object has a name");
            let there = id.clone();
            debug_assert_eq!(there, id);
            let (staged, located) = self.store.call_blocking({ let id = id.clone(); move |s| Ok((s.get(Table::Staging, &id)?, s.locate(Table::Staging, &id)?)) })?;
            if located.as_ref().is_some_and(|l| l.placed && l.rel == rel) {
                // Placed again where it is: the placement found it.
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            }
            if staged.is_none() {
                self.remove_in_place(rw, &parent, &name, run)?;
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            }
            // The daemon never moves or deletes in OneDrive for what is
            // leaving: such rows from before go; only content keeps it.
            let dropped = self.store.call_blocking({ let rel = rel.clone(); move |s| s.outbox_drop_moves(&rel) })?;
            if !dropped.is_empty() {
                tracing::info!("{} is no longer placed here: {} move(s) or delete(s) waiting for it are dropped", rel.display(), dropped.len());
            }
            if is_dir {
                if !located.as_ref().is_some_and(|l| l.placed) {
                    self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_refresh_items(&id) })?;
                }
                self.remove_gone_inside(rw, &dir, &name, &rel, run)?;
            }
            if self.keeps_leaving(rw, &rel, is_dir)? {
                continue;
            }
            // Placed elsewhere now, the item's own object is the new one: only
            // what is here is forgotten.
            let placed_elsewhere = located.is_some_and(|l| l.placed);
            let survey = self.forget_before_removing(&dir, &name, !placed_elsewhere)?;
            let removed = self.remove_whole(None, &dir, &name, &rel, run);
            if removed.is_err() {
                self.settle_stopped(&dir, &name, &survey);
            }
            removed?;
            tracing::info!("{} is no longer placed here, and nothing in it waits to be uploaded: removed", rel.display());
            self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
        }
        Ok(())
    }

    /// The leaving object of item `id` at `rel`: whether it is a directory,
    /// and the directory it is in. `None` when nothing is there (`ENOENT`) or
    /// another object is; any other error is returned.
    fn leaving_at(&self, rel: &Path, id: &str) -> std::io::Result<Option<(bool, File)>> {
        let gone = |e: &std::io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
        let (Some(name), parent) = (rel.file_name(), rel.parent().unwrap_or(Path::new(""))) else { return Ok(None) };
        let dir = match self.disk.dir(parent) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        // Where its handle is kept, only the object carrying it: another
        // with its id there — the copy placed again, moved there by the
        // user — is not it.
        let kept = self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_handle(&id) }).map_err(|e| std::io::Error::other(e.to_string()))?;
        // But for that, an object with its id at its place is it — an
        // editor's save by rename made a new inode — and its handle is
        // taken anew.
        let mut renewed = None;
        if let Some(kept) = kept {
            match konedrive_fs::handle::FileHandle::at(&dir, name) {
                Ok(there) if there == kept => {}
                Ok(there) => {
                    let elsewhere = self.store.call_blocking({ let (id, rel) = (id.to_owned(), rel.to_path_buf()); move |s| s.placed_elsewhere(&id, &rel) }).map_err(|e| std::io::Error::other(e.to_string()))?;
                    if elsewhere {
                        return Ok(None);
                    }
                    renewed = Some(there);
                }
                Err(e) if gone(&e) => return Ok(None),
                // No handle to compare (a filesystem that gives none): by
                // its id, as without one.
                Err(_) => {}
            }
        }
        match self.disk.probe(&dir, name) {
            Ok(Probe::Managed { id: there, is_dir }) if there == id => {
                if let Some(handle) = renewed {
                    self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_set_handle(&id, &handle) }).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(Some((is_dir, dir)))
            }
            Ok(_) => Ok(None),
            Err(e) if gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Where in the folder the leaving object of item `id` is, by the file
    /// handle taken when it began to leave: a walk of the folder, made only
    /// when its path no longer finds it.
    fn leaving_by_handle(&self, id: &str) -> Result<Option<PathBuf>, ApplyError> {
        let Some(handle) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_handle(&id) })? else { return Ok(None) };
        let root = self.disk.dir(Path::new(""))?;
        let dev = nix::sys::stat::fstat(root.as_fd()).map_err(std::io::Error::from)?.st_dev;
        let mut queue = std::collections::VecDeque::from([(PathBuf::new(), root)]);
        while let Some((rel, dir)) = queue.pop_front() {
            self.check_cancel()?;
            // A directory that cannot be read may hold it: nothing is decided.
            let names = match self.disk.list(&dir) {
                Ok(names) => names,
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                Err(e) => return Err(e.into()),
            };
            for name in names {
                // A file with other links: which name is the object's cannot
                // be told, so none is taken (the user's hard link).
                let single = || nix::sys::stat::fstatat(dir.as_fd(), name.as_os_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFDIR || s.st_nlink <= 1);
                if konedrive_fs::handle::FileHandle::at(&dir, &name).is_ok_and(|h| h == handle) && single() {
                    return Ok(Some(rel.join(&name)));
                }
                let is_dir = match nix::sys::stat::fstatat(dir.as_fd(), name.as_os_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
                    Err(nix::errno::Errno::ENOENT) => false,
                    Err(e) => return Err(std::io::Error::from(e).into()),
                };
                if is_dir {
                    match self.disk.open_subdir(&dir, &name) {
                        Ok(sub) => {
                            if nix::sys::stat::fstat(sub.as_fd()).is_ok_and(|s| s.st_dev == dev) {
                                queue.push_back((rel.join(&name), sub));
                            }
                        }
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(None)
    }

    /// Objects below the leaving folder at `dir/name` (at `rel`) whose items
    /// were in it when it began to leave and are gone from OneDrive since:
    /// removed as anything OneDrive removed, and their rows dropped — never
    /// uploaded again (issue #104, decision 2). An object with no item id
    /// (made here) is left to go up.
    fn remove_gone_inside(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let sub = self.disk.open_subdir(dir, name)?;
        if nix::sys::stat::fstat(sub.as_fd()).map_err(std::io::Error::from)?.st_dev != nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev {
            return Ok(());
        }
        for child in self.disk.list(&sub)? {
            self.check_cancel()?;
            let at = rel.join(&child);
            match self.disk.probe(&sub, &child)? {
                Probe::Managed { id, is_dir } => {
                    let gone = self.store.call_blocking({ let id = id.clone(); move |s| Ok(s.get(Table::Staging, &id)?.is_none() && s.leaving_had(&id)?) })?;
                    if gone {
                        let survey = self.forget_before_removing(&sub, &child, true)?;
                        // `resyncChangesUploadDifferences` does not mean removed: what
                        // was downloaded or changed here is kept, as anywhere (F116).
                        let removed = self.remove_whole(rw.upload_differences.then_some(rw), &sub, &child, &at, run);
                        if removed.is_err() {
                            self.settle_stopped(&sub, &child, &survey);
                        }
                        removed?;
                        let ids = survey.ids.clone();
                        let dropped = self.store.call_blocking({ let at = at.clone(); move |s| {
                            let mut dropped = s.outbox_drop_under(&at)?;
                            dropped.extend(s.outbox_drop_items(&ids)?);
                            Ok(dropped)
                        } })?;
                        tracing::info!("{} was removed from OneDrive while its folder was leaving: removed here, {} change(s) dropped", at.display(), dropped.len());
                    } else if is_dir {
                        self.remove_gone_inside(rw, &sub, &child, &at, run)?;
                    }
                }
                Probe::Unmanaged { is_dir: true } => self.remove_gone_inside(rw, &sub, &child, &at, run)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether what is leaving at `rel` stays on disk for now (review fixes
    /// 4 and 5 of issue #104): an examination of it, run here in this cycle,
    /// records or holds back something; an outbox row has a place at or
    /// below it; or it holds what cannot be told or removed — a file whose
    /// state cannot be read, another filesystem mounted inside — which the
    /// examination lists as not uploaded, with its reason.
    fn keeps_leaving(&self, rw: &Rw, rel: &Path, is_dir: bool) -> Result<bool, ApplyError> {
        let mut batch = crate::sync::local::Batch::new();
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            batch.name(parent, name);
        }
        if is_dir {
            batch.tree(rel);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let examined = crate::sync::local::Examiner {
            disk: &self.disk,
            store: &self.store,
            liveness: &crate::sync::local::NoLiveness,
            ignore: &rw.ignore,
            locks: &self.locks,
            now,
        }
        .examine(&batch)
        .map_err(|e| ApplyError::Io(format!("{} could not be examined before it goes: {e}", rel.display())))?;
        if !examined.applied.queued.is_empty() || !examined.recheck.is_empty() || !examined.undecided.is_empty() {
            tracing::debug!("{} waits: its examination recorded or held back something", rel.display());
            return Ok(true);
        }
        let (rows, skipped) = self.store.call_blocking({ let rel = rel.to_path_buf(); move |s| Ok((s.outbox_at_or_under(&rel)?, s.local_skipped()?)) })?;
        if !rows.is_empty() {
            tracing::debug!("{} waits for {} upload(s) before it goes", rel.display(), rows.len());
            return Ok(true);
        }
        use crate::sync::local::examine::{MOUNTED_INSIDE, UNKNOWN_STATE};
        if let Some(held) = skipped.iter().find(|k| k.rel.starts_with(rel) && (k.reason == MOUNTED_INSIDE || k.reason == UNKNOWN_STATE)) {
            tracing::info!("{} stays on disk: {} ({})", rel.display(), held.rel.display(), held.reason);
            return Ok(true);
        }
        Ok(false)
    }

    /// After a removal that failed: a file whose download was stopped for it
    /// and is still here is a placeholder again, never partly filled
    /// (review fix 6 of issue #104).
    pub(super) fn settle_stopped(&self, dir: &File, name: &OsStr, survey: &Survey) {
        if survey.stopped.is_empty() {
            return;
        }
        let mut files = Vec::new();
        let dev = match nix::sys::stat::fstat(dir.as_fd()) {
            Ok(stat) => stat.st_dev,
            Err(_) => return,
        };
        self.files_below(dir, name, dev, &mut files);
        // 10 s in all for the removal, not for each file: the cycle waits.
        let deadline = std::time::Instant::now() + SETTLE_WAIT;
        for file in files {
            let Ok(key) = crate::sync::InodeKey::of(&file) else { continue };
            if !survey.stopped.contains(&key) {
                continue;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // The fill lets go of the lock once it has stopped.
            let guard = self.runtime.block_on(async { tokio::time::timeout(left, self.locks.lock(key)).await });
            match guard {
                Ok(_guard) => crate::sync::source::back_to_placeholder(&file),
                Err(_) => tracing::warn!("a stopped download did not let go of its file in time; it is left as it is"),
            }
        }
    }

    /// The files at or below `dir/name`, opened, not into another filesystem.
    fn files_below(&self, dir: &File, name: &OsStr, dev: libc::dev_t, out: &mut Vec<File>) {
        match self.disk.probe(dir, name) {
            Ok(Probe::Managed { is_dir: false, .. } | Probe::Unmanaged { is_dir: false }) => {
                if let Ok(file) = self.disk.open_file(dir, name) {
                    out.push(file);
                }
            }
            Ok(Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true }) => {
                let Ok(sub) = self.disk.open_subdir(dir, name) else { return };
                if nix::sys::stat::fstat(sub.as_fd()).map(|s| s.st_dev) != Ok(dev) {
                    return;
                }
                for child in self.disk.list(&sub).unwrap_or_default() {
                    self.files_below(&sub, &child, dev, out);
                }
            }
            _ => {}
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
            let rebase = [OutboxOp::Rebase { from: rel.to_path_buf(), to: copy_rel.clone() }];
            self.store.call_blocking(move |s| s.outbox_apply(&rebase, 0))?;
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
                Some(id) => self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?.is_some_and(|l| l.placed),
                None => false,
            };
            if !placed && self.finish_new_folder(&holding, &name, id.as_deref(), run)? {
                continue;
            }
            // Removed in OneDrive: it goes (issue #104). One that is still
            // there but no longer placed goes back, and leaves from there.
            let removed = match &id {
                Some(id) => {
                    matches!(self.disk.probe(&holding, &name)?, Probe::Managed { id: ref there, .. } if there == id)
                        && self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?.is_none()
                }
                None => false,
            };
            if !placed && removed && self.remove_in_place(rw, Path::new(HOLDING), &name, run)? == Removal::Gone {
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
        if from.as_ref().is_some_and(|f| !is_new_name(f)) || self.store.call_blocking({ let id = id.to_owned(); move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some()) })? {
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
                if let Some(at) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(table, &id) })?.filter(|l| l.placed && !l.rel.as_os_str().is_empty()) {
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
                    // What is leaving inside it went along (issue #104).
                    let (from, to) = (PathBuf::from(HOLDING).join(name), parent.join(&candidate));
                    self.store.call_blocking(move |s| s.leaving_rebase(&from, &to))?;
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
