//! One row, one step (§3.6, §4, §5, §6): `mkdir`, `move` and `delete` here,
//! content in [`super::content`], and what they share — where the row's
//! object is, which folder it goes into, a name that is taken, the commit,
//! the conflict copy.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::State;

use super::engine::{now, outcome_of, Engine, Fail, Outcome};
use super::local::{self, Found};
use super::{kind, reason, Fault, SWAP_PREFIX};
use crate::drive::item::RESERVED_PREFIX;
use crate::drive::{DriveError, DriveItem, ItemChange, WriteError};
use crate::sync::disk::{Disk, Probe};
use crate::sync::local::{names, RECHECK};
use crate::tree::outbox::{frees, Base, Committed, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use crate::tree::{classify, ActivityRow, Change, Kind, Placement, Row, Table};

pub(super) async fn run(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Outcome {
    let result = match row.kind {
        OutboxKind::Create | OutboxKind::Update => super::content::run(e, disk, row).await,
        OutboxKind::Mkdir => mkdir(e, disk, row).await,
        OutboxKind::Move => moved(e, disk, row).await,
        OutboxKind::Delete => delete(e, row).await,
        // The object is downloaded before its item goes (WR5).
        OutboxKind::MoveOut => super::move_out::run(e, disk, row).await,
    };
    result.unwrap_or_else(outcome_of)
}

pub(super) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, Fail> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Fail::Io(io::Error::other(e)))?.map_err(Fail::Io)
}

/// The name of the row's local object: the last part of where the
/// examination saw it. A name OneDrive refuses is blocked here.
pub(super) fn local_name(row: &OutboxRow) -> Result<String, Fail> {
    let name = row.rel.file_name().ok_or(Fail::Now(Outcome::blocked("no-name")))?;
    let refused = |r: names::Refused| Fail::Now(Outcome::blocked(r.as_str()));
    let name = name.to_str().ok_or_else(|| refused(names::Refused::NotUtf8))?;
    if let Some(r) = names::refused(OsStr::new(name)) {
        return Err(refused(r));
    }
    Ok(name.to_owned())
}

/// Whether the row is taking its item to a temporary name (F55 (7)).
pub(super) fn in_swap(row: &OutboxRow) -> bool {
    row.target_name.as_deref().is_some_and(|n| n.starts_with(SWAP_PREFIX))
}

/// The name the row sends: the temporary one while it has one, else the
/// local object's.
pub(super) fn wanted_name(row: &OutboxRow, local: &str) -> String {
    row.target_name.clone().filter(|_| in_swap(row)).unwrap_or_else(|| local.to_owned())
}

/// `.konedrive-swap-<item id>`, or `-s<seq>` for what has no id yet.
pub(super) fn swap_name(row: &OutboxRow) -> String {
    format!("{SWAP_PREFIX}{}", row.item_id.clone().unwrap_or_else(|| format!("s{}", row.seq)))
}

/// The item id of the directory `dir` (relative to the root), read from the
/// disk: the root's is the drive's root.
pub(super) fn dir_id(e: &Engine, disk: &Disk, dir: &Path) -> Result<Option<String>, Fail> {
    if dir.as_os_str().is_empty() {
        return Ok(e.store().with(|s| s.root_item_id())?);
    }
    let Some(name) = dir.file_name() else { return Ok(None) };
    let parent = match disk.dir(dir.parent().unwrap_or(Path::new(""))) {
        Ok(parent) => parent,
        Err(err) if matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(match disk.probe(&parent, name)? {
        Probe::Managed { id, is_dir: true } => Some(id),
        _ => None,
    })
}

/// The folder in OneDrive the row's item goes into: the one the examination
/// named, or — where that folder was still to be made — the one its
/// directory is now.
pub(super) fn parent_of(e: &Engine, disk: &Disk, row: &OutboxRow) -> Result<Option<String>, Fail> {
    if let Some(parent) = &row.target_parent {
        return Ok(Some(parent.clone()));
    }
    dir_id(e, disk, row.rel.parent().unwrap_or(Path::new("")))
}

/// The row's local object: where the row saw it, or where a row behind it
/// saw it since. `None` when it is in neither place.
pub(super) fn locate(e: &Engine, disk: &Disk, row: &OutboxRow) -> Result<Option<Found>, Fail> {
    let others = e.store().with(|s| match (&row.item_id, &row.inode) {
        (Some(id), _) => s.outbox_for_item(id),
        (None, Some(inode)) => s.outbox_for_inode(inode),
        _ => Ok(Vec::new()),
    })?;
    let places = std::iter::once(row.rel.clone()).chain(others.into_iter().filter(|r| r.seq != row.seq).map(|r| r.rel));
    for rel in places {
        if let Some(found) = local::find(disk, &rel)? {
            if row.inode.as_ref().is_none_or(|inode| inode.same_object(&found.inode)) {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// The base row Graph's answer makes.
pub(super) fn answer_row(item: &DriveItem, parent: Option<&str>) -> Result<Row, Fail> {
    match classify(item) {
        Change::Upsert(mut row) => {
            if row.parent_id.is_none() {
                row.parent_id = parent.map(str::to_owned);
            }
            Ok(row)
        }
        _ => Err(Fail::Io(io::Error::other(format!("OneDrive answered with {} as deleted, or as the root", item.id)))),
    }
}

fn place(item: &DriveItem) -> (Option<String>, String) {
    (item.parent_reference.as_ref().and_then(|p| p.id.clone()), item.name.clone().unwrap_or_default())
}

/// Commit step 2 (§3.5), or its temporary form: the item landed under a
/// temporary name, and a `move` row takes it on to the local name.
pub(super) fn commit_row(e: &Engine, row: &OutboxRow, answer: &Row, handle: Option<&FileHandle>, parent: &str, event: ActivityRow) -> Result<(), Fail> {
    if in_swap(row) {
        let final_name = local_name(row)?;
        e.store().with(|s| s.outbox_commit_temporary(row.seq, answer, handle, parent, &final_name, Some(&event)))?;
    } else {
        e.store().with(|s| s.outbox_commit(row.seq, Committed::Item { row: answer, handle }, Some(&event)))?;
    }
    e.cfg.host.activity(&event);
    Ok(())
}

/// What the row would have made, to recognise it at a taken name.
pub(super) enum Ours<'a> {
    Folder,
    /// A file with this content (quickXorHash).
    File(&'a str),
    /// This item, moved.
    Item(&'a str),
}

pub(super) enum Taken {
    /// Nothing holds the name any more: send again.
    Free,
    /// Held by an item a live row is taking away (F55 (7)): take this
    /// temporary name instead.
    Temporary(String),
    /// What stands there is what the row makes: its own earlier request, or
    /// the same content (§5, §6 create/create).
    Adopt(Box<DriveItem>),
    /// Something else: keep both (§6).
    Copy,
}

/// A `409` for a row that takes (`parent`, `name`) in OneDrive: what holds
/// it? An item that a live row frees from that place — in any state, the
/// names compared without case — holds it only for now: it is never
/// adopted, never copied, and the name is not tried again (F55 (7)). Only
/// then is what stands there compared with what the row makes, by id, so a
/// replay still adopts its own folder. An item this machine already knows
/// (one with a live row, or placed here) is never adopted by another local
/// object: that would give two objects one id.
pub(super) async fn taken(e: &Engine, row: &OutboxRow, parent: &str, name: &str, ours: Ours<'_>) -> Result<Taken, Fail> {
    let holder = match e.cfg.drive.child(parent, name).await {
        Ok(holder) => holder,
        Err(DriveError::NotFound) => return Ok(Taken::Free),
        Err(err) => return Err(err.into()),
    };
    let own_item = matches!(ours, Ours::Item(id) if id == holder.id);
    let is_ours = match ours {
        Ours::Folder => holder.folder.is_some(),
        Ours::File(hash) => holder.file.is_some() && holder.quick_xor_hash() == Some(hash),
        Ours::Item(_) => own_item && holder.name.as_deref() == Some(name),
    };
    if name.starts_with(SWAP_PREFIX) {
        // Its own temporary name: a replay adopts what it made there.
        return Ok(if is_ours { Taken::Adopt(Box::new(holder)) } else { Taken::Temporary(format!("{name}-{}", row.seq)) });
    }
    let rows = e.store().with(|s| s.outbox_rows())?;
    let lower = name.to_lowercase();
    let freed = rows.iter().any(|r| {
        r.seq != row.seq
            && r.item_id.as_deref() == Some(holder.id.as_str())
            && frees(r).is_some_and(|(p, n)| p == parent && n.to_lowercase() == lower)
    });
    // A case-only rename OneDrive would not take: this item holds the name.
    let own_case = own_item && !is_ours;
    if freed || own_case {
        tracing::info!("{name} is still another item's in OneDrive: {} goes through {}", row.rel.display(), swap_name(row));
        return Ok(Taken::Temporary(swap_name(row)));
    }
    let known_here = !own_item
        && (rows.iter().any(|r| r.item_id.as_deref() == Some(holder.id.as_str())) || e.store().with(|s| s.local_handle(&holder.id))?.is_some());
    Ok(if is_ours && !known_here { Taken::Adopt(Box::new(holder)) } else { Taken::Copy })
}

/// The row goes to `swap` first (saved before it is sent, WR7).
pub(super) fn temporary(e: &Engine, row: &OutboxRow, parent: &str, swap: &str) -> Result<Outcome, Fail> {
    e.store().with(|s| s.outbox_set_target(row.seq, Some(parent), Some(swap)))?;
    Ok(Outcome::again())
}

/// §6's copy: the local object renamed beside what OneDrive holds at its
/// name (`<stem>-<machine><.ext>`, never over anything), konedrive's
/// attributes taken off, recorded as a conflict of kind `copy`. The row
/// becomes the copy's create (or mkdir) — or, for a move, the move to the
/// copy's name. `forget` is the item the copy was made from: its name is
/// placed again from the cloud, never deleted there.
pub(super) async fn copy(e: &Engine, disk: &Disk, row: &OutboxRow, found: &Found, parent: &str, forget: Option<&str>) -> Result<Outcome, Fail> {
    let (event, copy_rel) = {
        let _tree = e.cfg.tree_lock.lock().await;
        let copy_name = local::rename_to_copy(disk, found, &e.cfg.machine_name)?;
        let copy_rel = found.rel.with_file_name(&copy_name);
        let moving = row.kind == OutboxKind::Move;
        if !moving {
            if let Some(copied) = local::find(disk, &copy_rel)? {
                if copied.is_dir {
                    local::strip(&copied.open_dir()?)?;
                } else {
                    local::strip(&copied.open()?)?;
                }
            }
        }
        let original = e.cfg.root.path.join(&found.rel).display().to_string();
        let copy_path = e.cfg.root.path.join(&copy_rel).display().to_string();
        let event = e.event(kind::CONFLICT, &found.rel, copy_path.clone());
        let (inode, is_dir, rel, parent, name) = (found.inode.clone(), found.is_dir, copy_rel.clone(), parent.to_owned(), copy_name.clone());
        let amend = move |next: &mut OutboxRow| {
            next.rel = rel;
            next.inode = Some(inode);
            next.target_parent = Some(parent);
            next.target_name = Some(name);
            next.state = OutboxState::Running;
            next.reason = None;
            next.attempts = 0;
            next.next_try = None;
            next.snapshot = None;
            next.session_url = None;
            next.session_expires = None;
            next.session_next = None;
            if !moving {
                next.kind = if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create };
                next.item_id = None;
                next.base = None;
            }
        };
        e.store().with(|s| s.outbox_copied(row.seq, amend, forget, now(), &original, &copy_path, Some(&event)))?;
        if found.is_dir {
            e.store().with(|s| s.outbox_apply(&[OutboxOp::Rebase { from: found.rel.clone(), to: copy_rel.clone() }], now()))?;
        }
        (event, copy_rel)
    };
    if let Some(url) = &row.session_url {
        if let Err(err) = e.cfg.drive.cancel_upload(url).await {
            tracing::debug!("an abandoned upload session was not cancelled: {err}");
        }
    }
    tracing::info!("{} was changed in OneDrive too: the local version is kept as {}", found.rel.display(), copy_rel.display());
    e.cfg.host.activity(&event);
    e.cfg.host.cycle_wanted();
    Ok(Outcome::again())
}

/// Rename × rename (§6): the first to reach OneDrive wins, so the local
/// object goes where OneDrive has it. `None` where it cannot or must not:
/// OneDrive's folder is not placed here, the name is taken here, or it is a
/// name no listing places — the daemon's own `.konedrive-*` (a temporary
/// step of this very row, I1), a name OneDrive keeps but Linux cannot, or
/// one OneDrive refuses. The local place then stands. The caller holds the
/// tree lock.
pub(super) fn follow_cloud(e: &Engine, disk: &Disk, found: &Found, remote: &DriveItem) -> Result<Option<PathBuf>, Fail> {
    let (Some(parent), name) = place(remote) else { return Ok(None) };
    let placeable = matches!(classify(remote), Change::Upsert(row) if row.placement == Placement::Placed);
    if !placeable || name.starts_with(RESERVED_PREFIX) || names::refused(OsStr::new(&name)).is_some() {
        return Ok(None);
    }
    let Some(dir_rel) = e.store().with(|s| s.locate(Table::Items, &parent))?.filter(|l| l.placed).map(|l| l.rel) else {
        return Ok(None);
    };
    let to_rel = dir_rel.join(&name);
    if to_rel == found.rel {
        return Ok(Some(to_rel));
    }
    match disk.dir(&dir_rel).and_then(|to| disk.rename(&found.dir, &found.name, &to, OsStr::new(&name))) {
        Ok(()) => {
            if found.is_dir {
                e.store().with(|s| s.outbox_apply(&[OutboxOp::Rebase { from: found.rel.clone(), to: to_rel.clone() }], now()))?;
            }
            tracing::info!("{} was renamed in OneDrive first: it is {} here too", found.rel.display(), to_rel.display());
            Ok(Some(to_rel))
        }
        Err(err) => {
            tracing::info!("{} keeps its local name ({err}): OneDrive's rename is undone", found.rel.display());
            Ok(None)
        }
    }
}

/// Uploads the local object again as new (§6: edit/delete, move/delete —
/// local wins): the base forgets the item, the file loses konedrive's
/// attributes and becomes a `create` at its local place; the id changes.
pub(super) async fn upload_as_new(e: &Engine, row: &OutboxRow, found: &Found, parent: &str, id: &str) -> Result<Outcome, Fail> {
    let _tree = e.cfg.tree_lock.lock().await;
    let is_dir = found.is_dir;
    if is_dir {
        local::strip(&found.open_dir()?)?;
    } else {
        local::strip(&found.open()?)?;
    }
    let (inode, rel, parent, name) = (found.inode.clone(), found.rel.clone(), parent.to_owned(), found.name.to_str().map(str::to_owned));
    let amend = move |next: &mut OutboxRow| {
        next.kind = if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create };
        next.item_id = None;
        next.inode = Some(inode);
        next.rel = rel;
        next.base = None;
        next.target_parent = Some(parent);
        next.target_name = name;
        next.reason = None;
        next.attempts = 0;
        next.next_try = None;
        next.snapshot = None;
        next.session_url = None;
        next.session_expires = None;
        next.session_next = None;
    };
    let event = e.event(kind::RESTORED, &found.rel, "deleted in OneDrive while it was changed here: uploaded again");
    e.store().with(|s| s.outbox_orphan(id, row.seq, amend, Some(&event)))?;
    e.cfg.host.activity(&event);
    e.cfg.host.cycle_wanted();
    Ok(Outcome::again())
}

async fn mkdir(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let local = local_name(&row)?;
    let Some(found) = locate(e, disk, &row)?.filter(|f| f.is_dir) else { return Ok(Outcome::later(reason::NOT_FOUND, RECHECK)) };
    let Some(parent) = parent_of(e, disk, &row)? else { return Ok(Outcome::later(reason::PARENT, RECHECK)) };
    let name = wanted_name(&row, &local);
    match e.cfg.drive.create_folder(&parent, &name).await {
        Ok(item) => {
            e.fault(Fault::AfterSend)?;
            commit_dir(e, &row, &found, &item, &parent).await
        }
        Err(WriteError::NameExists) => match taken(e, &row, &parent, &name, Ours::Folder).await? {
            Taken::Free => Ok(Outcome::again()),
            Taken::Temporary(swap) => temporary(e, &row, &parent, &swap),
            // A folder of that name: adopted, and the contents merge file by
            // file (§4.2).
            Taken::Adopt(item) => commit_dir(e, &row, &found, &item, &parent).await,
            Taken::Copy => copy(e, disk, &row, &found, &parent, None).await,
        },
        Err(WriteError::NotFound) => {
            e.cfg.host.cycle_wanted();
            Ok(Outcome::backoff(reason::PARENT))
        }
        Err(other) => Err(other.into()),
    }
}

async fn commit_dir(e: &Engine, row: &OutboxRow, found: &Found, item: &DriveItem, parent: &str) -> Result<Outcome, Fail> {
    let answer = answer_row(item, Some(parent))?;
    let _tree = e.cfg.tree_lock.lock().await;
    let dir = found.open_dir()?;
    let id = item.id.clone();
    blocking(move || local::commit_dir(&dir, &id)).await?;
    e.fault(Fault::AfterCommitStep1)?;
    let event = e.event(kind::UPLOADED, &found.rel, "folder");
    commit_row(e, row, &answer, found.inode.handle.as_ref(), parent, event)?;
    Ok(Outcome::Done)
}

async fn moved(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let (Some(id), Some(base)) = (row.item_id.clone(), row.base.clone()) else { return Ok(Outcome::blocked("no-item")) };
    let local = local_name(&row)?;
    let Some(parent) = parent_of(e, disk, &row)? else { return Ok(Outcome::later(reason::PARENT, RECHECK)) };
    let name = wanted_name(&row, &local);
    let found = locate(e, disk, &row)?;
    let Some(guard) = base.etag.clone().or_else(|| base.ctag.clone()) else { return Ok(Outcome::blocked("no-guard")) };
    let change = ItemChange {
        name: (Some(name.as_str()) != base.name.as_deref()).then_some(name.as_str()),
        parent_id: (Some(parent.as_str()) != base.parent.as_deref()).then_some(parent.as_str()),
        modified: None,
    };
    if change.name.is_none() && change.parent_id.is_none() {
        if in_swap(&row) {
            // Already under its temporary name (an answer lost, then the
            // merge): the temporary step is committed, so that its final
            // move follows.
            let remote = e.cfg.drive.item(&id).await?;
            return commit_move(e, &row, found.as_ref(), &remote, &parent).await;
        }
        // Where the base has it already: nothing to send.
        e.store().with(|s| s.outbox_drop(row.seq, None, None, None))?;
        return Ok(Outcome::Done);
    }
    match e.cfg.drive.update_item(&id, &guard, &change).await {
        Ok(item) => {
            e.fault(Fault::AfterSend)?;
            commit_move(e, &row, found.as_ref(), &item, &parent).await
        }
        Err(WriteError::NameExists) => match taken(e, &row, &parent, &name, Ours::Item(&id)).await? {
            Taken::Free => Ok(Outcome::again()),
            Taken::Temporary(swap) => temporary(e, &row, &parent, &swap),
            Taken::Adopt(item) => commit_move(e, &row, found.as_ref(), &item, &parent).await,
            Taken::Copy => match &found {
                Some(found) => copy(e, disk, &row, found, &parent, None).await,
                None => Ok(Outcome::later(reason::NOT_FOUND, RECHECK)),
            },
        },
        Err(WriteError::Changed) => {
            let remote = match e.cfg.drive.item(&id).await {
                Ok(remote) => remote,
                Err(DriveError::NotFound) => return move_gone(e, disk, &row, found.as_ref(), &id, &parent).await,
                Err(err) => return Err(err.into()),
            };
            let (remote_parent, remote_name) = place(&remote);
            if remote_parent.as_deref() == Some(parent.as_str()) && remote_name == name {
                // It went through before (§5: a PATCH sent, no answer).
                return commit_move(e, &row, found.as_ref(), &remote, &parent).await;
            }
            if remote_parent.as_deref() != base.parent.as_deref() || Some(remote_name.as_str()) != base.name.as_deref() {
                // Moved there as well: the first to reach OneDrive wins (§6).
                if let Some(found) = &found {
                    let _tree = e.cfg.tree_lock.lock().await;
                    if let Some(to_rel) = follow_cloud(e, disk, found, &remote)? {
                        let answer = answer_row(&remote, None)?;
                        let handle = found.inode.handle.clone();
                        local::mark(disk, &to_rel, None);
                        let event = e.event("moved", &to_rel, format!("renamed in OneDrive first; was {} here", found.rel.display()));
                        e.store().with(|s| s.outbox_commit(row.seq, Committed::Item { row: &answer, handle: handle.as_ref() }, Some(&event)))?;
                        e.cfg.host.activity(&event);
                        return Ok(Outcome::Done);
                    }
                }
            }
            // Changed there, not moved (or its place cannot be followed here,
            // such as its own temporary name): the move goes again against
            // the fresh eTag; the delta brings the content (§6).
            let fresh = Base { etag: remote.e_tag.clone(), ctag: base.ctag.clone(), parent: remote_parent, name: Some(remote_name) };
            e.store().with(|s| s.outbox_amend(row.seq, |next| next.base = Some(fresh)))?;
            Ok(Outcome::again())
        }
        Err(WriteError::NotFound) => move_gone(e, disk, &row, found.as_ref(), &id, &parent).await,
        Err(other) => Err(other.into()),
    }
}

async fn commit_move(e: &Engine, row: &OutboxRow, found: Option<&Found>, item: &DriveItem, parent: &str) -> Result<Outcome, Fail> {
    let answer = answer_row(item, Some(parent))?;
    let was = match &row.item_id {
        Some(id) => e.store().with(|s| s.locate(Table::Items, id))?.map(|l| l.rel.display().to_string()).unwrap_or_default(),
        None => String::new(),
    };
    let _tree = e.cfg.tree_lock.lock().await;
    let handle = found.and_then(|f| f.inode.handle.clone()).or_else(|| row.inode.as_ref().and_then(|i| i.handle.clone()));
    let rel = found.map(|f| f.rel.as_path()).unwrap_or(&row.rel);
    if let Some(found) = found {
        local::clear_mark(found);
    }
    let event = e.event(kind::CLOUD_MOVED, rel, was);
    commit_row(e, row, &answer, handle.as_ref(), parent, event)?;
    Ok(Outcome::Done)
}

/// A `404` for a move: gone from OneDrive meanwhile (§6, move/delete).
/// Content decides: a downloaded file, or a folder, is uploaded again as
/// new at its new place; a placeholder, which holds nothing here, follows
/// the delete.
async fn move_gone(e: &Engine, disk: &Disk, row: &OutboxRow, found: Option<&Found>, id: &str, parent: &str) -> Result<Outcome, Fail> {
    let Some(found) = found else {
        return gone(e, row, id, "deleted in OneDrive").await;
    };
    if found.is_dir {
        return upload_as_new(e, row, found, parent, id).await;
    }
    match found.state()? {
        Some(State::OnlineOnly) => {
            let _tree = e.cfg.tree_lock.lock().await;
            // Still the same placeholder, holding nothing, and nobody has it
            // open (a write lease, as a free-up takes): removed.
            let file = found.open()?;
            let Some(lease) = WriteLease::take(&file)? else { return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)) };
            let again = local::find(disk, &found.rel)?;
            if again.as_ref().is_some_and(|a| a.inode.same_object(&found.inode)) && found.state()? == Some(State::OnlineOnly) {
                disk.remove(&found.dir, &found.name, false)?;
            }
            drop(lease);
            let event = e.event(kind::CLOUD_DELETED, &found.rel, "deleted in OneDrive; the placeholder here went too");
            e.store().with(|s| s.outbox_commit(row.seq, Committed::Gone { item_id: id }, Some(&event)))?;
            e.cfg.host.activity(&event);
            Ok(Outcome::Done)
        }
        Some(State::Hydrated) | None => upload_as_new(e, row, found, parent, id).await,
        Some(_) => Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)),
    }
}

pub(super) async fn delete(e: &Arc<Engine>, row: OutboxRow) -> Result<Outcome, Fail> {
    let Some(id) = row.item_id.clone() else { return Ok(Outcome::blocked("no-item")) };
    let base = row.base.clone().unwrap_or_default();
    let folder = e.store().with(|s| s.get(Table::Items, &id))?.is_some_and(|item| item.kind == Kind::Folder);
    if folder {
        return delete_folder(e, &row, &id).await;
    }
    let Some(guard) = base.etag.clone().or_else(|| base.ctag.clone()) else { return Ok(Outcome::blocked("no-guard")) };
    match e.cfg.drive.delete_item(&id, &guard).await {
        Ok(()) => {
            e.fault(Fault::AfterSend)?;
            gone(e, &row, &id, "to OneDrive's recycle bin").await
        }
        // Gone already (§5: a DELETE sent, no answer).
        Err(WriteError::NotFound) => gone(e, &row, &id, "to OneDrive's recycle bin").await,
        Err(WriteError::Changed) => file_changed(e, &row, &id, &base).await,
        Err(other) => Err(other.into()),
    }
}

/// Commit step 2 of a delete, under the tree lock.
async fn gone(e: &Engine, row: &OutboxRow, id: &str, why: &str) -> Result<Outcome, Fail> {
    let event = e.event(kind::CLOUD_DELETED, &row.rel, why);
    let _tree = e.cfg.tree_lock.lock().await;
    e.store().with(|s| s.outbox_commit(row.seq, Committed::Gone { item_id: id }, Some(&event)))?;
    e.cfg.host.activity(&event);
    Ok(Outcome::Done)
}

/// OneDrive's version comes back: the delete is dropped, and the item is
/// placed again by the reconcile (§6, delete × edit: remote wins). Under the
/// tree lock, so that a cycle's swap cannot give the item its old local
/// object back.
async fn restored(e: &Engine, row: &OutboxRow, id: &str, why: &str) -> Result<Outcome, Fail> {
    let event = e.event(kind::RESTORED, &row.rel, why);
    {
        let _tree = e.cfg.tree_lock.lock().await;
        e.store().with(|s| s.outbox_drop(row.seq, None, Some(id), Some(&event)))?;
    }
    e.cfg.host.activity(&event);
    e.cfg.host.full_cycle_wanted();
    Ok(Outcome::Done)
}

/// `412` on a file's delete: only its name or place changed there — it is
/// the content the user deleted, and goes with the fresh eTag — or its
/// content changed, and OneDrive's version comes back (§6).
async fn file_changed(e: &Engine, row: &OutboxRow, id: &str, base: &Base) -> Result<Outcome, Fail> {
    for _ in 0..3 {
        let remote = match e.cfg.drive.item(id).await {
            Ok(remote) => remote,
            Err(DriveError::NotFound) => return gone(e, row, id, "to OneDrive's recycle bin").await,
            Err(err) => return Err(err.into()),
        };
        if remote.c_tag.is_none() || remote.c_tag != base.ctag {
            return restored(e, row, id, "changed in OneDrive after it was deleted here").await;
        }
        let guard = remote.e_tag.clone().or(remote.c_tag.clone()).unwrap_or_default();
        match e.cfg.drive.delete_item(id, &guard).await {
            Ok(()) | Err(WriteError::NotFound) => return gone(e, row, id, "to OneDrive's recycle bin").await,
            Err(WriteError::Changed) => continue,
            Err(other) => return Err(other.into()),
        }
    }
    Ok(Outcome::backoff("changed in OneDrive again and again"))
}

/// A folder's delete (§4.7): one `DELETE` of the whole folder, unguarded —
/// no `If-Match`, whatever changed inside it in OneDrive since. As on
/// Windows, the folder goes to the recycle bin whole; the recycle bin is the
/// safety net ([decisions.md](../../../../docs/design/decisions.md), "A
/// folder delete is the whole folder, as on Windows").
async fn delete_folder(e: &Engine, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    match e.cfg.drive.delete_folder(id).await {
        Ok(()) => {
            e.fault(Fault::AfterSend)?;
            gone(e, row, id, "to OneDrive's recycle bin").await
        }
        // Gone already (§5: a DELETE sent, no answer).
        Err(WriteError::NotFound) => gone(e, row, id, "to OneDrive's recycle bin").await,
        Err(other) => Err(other.into()),
    }
}
