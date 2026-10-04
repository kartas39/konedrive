//! What the steps share: where the row's object is, which folder it goes
//! into, a name that is taken, the guard of a request, the commit, the
//! conflict copy, and the end of a row whose object is gone.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::handle::FileHandle;

use super::sections::{blocking, blocking_under, tree, Tree};
use crate::folder::classify::classify;
use crate::folder::disk::{Disk, Probe};
use crate::local::{names, RECHECK};
use crate::upload::engine::{now, Engine, Fail, Outcome};
use crate::upload::local::{self, Found};
use crate::upload::{kind, SWAP_PREFIX};
use konedrive_fs::RESERVED_PREFIX;
use konedrive_graph::drive::{DriveError, DriveItem, WriteError};
use konedrive_tree::outbox::{frees, Base, Committed, ConflictCopy, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason, SessionUrl};
use konedrive_tree::{ActivityRow, Change, Kind, Placement, Row, Table};

/// The name of the row's local object: the last part of where the
/// examination saw it. A name OneDrive refuses is blocked here.
pub(in crate::upload) fn local_name(row: &OutboxRow) -> Result<String, Fail> {
    let name = row.rel.file_name().ok_or(Fail::Now(Outcome::blocked(Reason::NoName)))?;
    let refused = |r: names::Refused| Fail::Now(Outcome::blocked(r.reason()));
    let name = name.to_str().ok_or_else(|| refused(names::Refused::NotUtf8))?;
    if let Some(r) = names::refused(OsStr::new(name)) {
        return Err(refused(r));
    }
    Ok(name.to_owned())
}

/// Whether the row is taking its item to a temporary name (F55 (7)).
pub(super) fn in_swap(row: &OutboxRow) -> bool {
    row.swap_name().is_some()
}

/// The name the row sends: the temporary one while it has one, else the
/// local object's.
pub(in crate::upload) fn wanted_name(row: &OutboxRow, local: &str) -> String {
    row.swap_name().unwrap_or(local).to_owned()
}

/// `.konedrive-swap-<item id>`, or `-s<seq>` for what has no id yet.
fn swap_name(row: &OutboxRow) -> String {
    format!("{SWAP_PREFIX}{}", row.item_id.clone().unwrap_or_else(|| format!("s{}", row.seq)))
}

/// The item id of the directory `dir` (relative to the root), read from the
/// disk: the root's is the drive's root. Only an id that is the directory's
/// own counts: the base has it as a folder, and records this very object for
/// it (or, with no object recorded, places it here; what is leaving counts
/// by the object or the place `leaving` keeps). A copy that kept its
/// attributes, or a folder from elsewhere, carries an id that names another
/// folder in OneDrive; nothing is sent into that one because of it. The
/// examination strips such a directory when it can, but it does not always
/// see it (a batch that names only what is below it), and cannot always strip
/// it (`LO3`): this is the one place every row passes before it is sent.
async fn dir_id(e: &Engine, disk: &Arc<Disk>, dir: &Path) -> Result<Option<String>, Fail> {
    if dir.as_os_str().is_empty() {
        return Ok(e.store().call(|s| s.root_item_id()).await?);
    }
    let Some(name) = dir.file_name().map(OsStr::to_owned) else { return Ok(None) };
    let (on, at) = (Arc::clone(disk), dir.to_owned());
    let probed = blocking(move || {
        let parent = match on.dir(at.parent().unwrap_or(Path::new(""))) {
            Ok(parent) => parent,
            Err(err) if matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Ok(None),
            Err(err) => return Err(err),
        };
        let Probe::Managed { id, is_dir: true } = on.probe(&parent, &name)? else { return Ok(None) };
        Ok(Some((id, FileHandle::at(&parent, &name).ok())))
    })
    .await?;
    let Some((id, here)) = probed else { return Ok(None) };
    let (asked, at) = (id.clone(), dir.to_owned());
    let own = e
        .store()
        .call(move |s| {
            if !s.get(Table::Items, &asked)?.is_some_and(|row| row.kind == Kind::Folder) {
                return Ok(false);
            }
            // The object the base records for it, or the one kept while it
            // is leaving: a folder that leaves and is placed again elsewhere
            // meanwhile has both, and the leaving one still takes what waits
            // inside it (issue #104).
            let (placed, left) = (s.local_handle(&asked)?, s.leaving_handle(&asked)?);
            if here.is_some() && (placed == here || left == here) {
                return Ok(true);
            }
            // With no object to compare: where the base places it.
            if (placed.is_none() || here.is_none()) && s.locate(Table::Items, &asked)?.is_some_and(|l| l.placed && l.rel == at) {
                return Ok(true);
            }
            // What leaves stays where it is. The leaving folder itself, when
            // no object was kept for it, by its place; a folder that was
            // inside one has no object of its own there (the base's, if any,
            // is the copy placed again), and counts below the leaving place.
            if !s.leaving_had(&asked)? {
                return Ok(false);
            }
            Ok(s.leaving()?.iter().any(|(id, rel)| if *id == asked { left.is_none() && at == *rel } else { at != *rel && at.starts_with(rel) }))
        })
        .await?;
    if !own {
        tracing::debug!("{} carries the id of another folder ({id}); nothing is sent into that one for it", dir.display());
    }
    Ok(own.then_some(id))
}

/// The folder in OneDrive the row's item goes into: the one the examination
/// named, or — where that folder was still to be made — the one its
/// directory is now.
pub(in crate::upload) async fn parent_of(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<Option<String>, Fail> {
    if let Some(parent) = &row.target_parent {
        return Ok(Some(parent.clone()));
    }
    dir_id(e, disk, row.rel.parent().unwrap_or(Path::new(""))).await
}

/// The row's local object: where the row saw it, or where a row behind it
/// saw it since. `None` when it is in neither place.
pub(in crate::upload) async fn locate(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<Option<Found>, Fail> {
    let (item_id, inode) = (row.item_id.clone(), row.inode.clone());
    let others = e.store().call(move |s| match (&item_id, &inode) {
        (Some(id), _) => s.outbox_for_item(id),
        (None, Some(inode)) => s.outbox_for_inode(inode),
        _ => Ok(Vec::new()),
    })
    .await?;
    let places: Vec<PathBuf> = std::iter::once(row.rel.clone()).chain(others.into_iter().filter(|r| r.seq != row.seq).map(|r| r.rel)).collect();
    let (disk, inode) = (Arc::clone(disk), row.inode.clone());
    blocking(move || {
        for rel in places {
            if let Some(found) = local::find(&disk, &rel)? {
                if inode.as_ref().is_none_or(|inode| inode.same_object(&found.inode)) {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    })
    .await
}

/// The base row Graph's answer makes.
pub(in crate::upload) fn answer_row(item: &DriveItem, parent: Option<&str>) -> Result<Row, Fail> {
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

/// Where OneDrive has `item`: its parent's id and its name.
pub(super) fn place(item: &DriveItem) -> (Option<String>, String) {
    (item.parent_reference.as_ref().and_then(|p| p.id.clone()), item.name.clone().unwrap_or_default())
}

/// Commit step 2 (§3.5), or its temporary form: the item landed under a
/// temporary name, and a `move` row takes it on to the local name.
pub(in crate::upload) async fn commit_row(e: &Engine, row: &OutboxRow, answer: &Row, handle: Option<&FileHandle>, parent: &str, event: ActivityRow) -> Result<(), Fail> {
    let (seq, answer, handle, parent, stored) = (row.seq, answer.clone(), handle.cloned(), parent.to_owned(), event.clone());
    if in_swap(row) {
        let final_name = local_name(row)?;
        e.store().call(move |s| s.outbox_commit_temporary(seq, &answer, handle.as_ref(), &parent, &final_name, Some(&stored))).await?;
    } else {
        e.store().call(move |s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: handle.as_ref() }, Some(&stored))).await?;
    }
    e.host().activity(&event);
    Ok(())
}

/// The `If-Match` of a guarded request (WR2): an item's eTag, or its cTag
/// where no eTag is known (F55 (4): a row queued against another version than
/// the base's carries that version's cTag, and no eTag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::upload) struct Guard(String);

impl Guard {
    /// The eTag, else the cTag; `None` with neither.
    pub(in crate::upload) fn of(etag: Option<&str>, ctag: Option<&str>) -> Option<Self> {
        etag.or(ctag).map(|tag| Self(tag.to_owned()))
    }

    /// The guard of the version the row's change was made against. A row
    /// with none is blocked ([`Reason::NoGuard`]): nothing is sent for it.
    pub(in crate::upload) fn of_base(base: &Base) -> Option<Self> {
        Self::of(base.etag.as_deref(), base.ctag.as_deref())
    }

    /// The guard of an item as OneDrive just answered it, for the delete
    /// that follows the read. An answer with neither tag gives an empty
    /// guard, and the request goes out with an empty `If-Match`: what
    /// OneDrive makes of that was not measured (limitations log F235).
    pub(in crate::upload) fn of_item(item: &DriveItem) -> Self {
        Self::of(item.e_tag.as_deref(), item.c_tag.as_deref()).unwrap_or(Self(String::new()))
    }

    /// This guard, or the eTag an answer gave since.
    pub(in crate::upload) fn renewed(self, etag: Option<String>) -> Self {
        etag.map_or(self, Self)
    }

    pub(in crate::upload) fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the row would have made, to recognise it at a taken name.
pub(in crate::upload) enum Ours<'a> {
    Folder,
    /// A file with this content (quickXorHash).
    File(&'a str),
    /// This item, moved.
    Item(&'a str),
    /// A file of this size and time (Unix seconds): the content a row sent
    /// whose file is gone and cannot be hashed any more.
    Sent { size: u64, mtime: i64 },
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
    /// An empty file the delta feed never listed: as far as anything here
    /// can tell, the placeholder of an upload session — another device's,
    /// one abandoned, or one of this folder's (issue #89). Never copied
    /// around, never deleted (a delete ends its session): the row waits
    /// ([`Reason::NameHeld`]).
    Held,
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
///
/// What would otherwise be a copy is [`Taken::Held`] when the holder is an
/// empty file the items table (the delta feed's mirror) does not know: an
/// upload session's placeholder is never in the feed (nor in a listing being
/// staged), and nothing in OneDrive
/// tells a live session from an abandoned one, or whose it is (issue #89). An
/// empty file the feed listed is a real file, and decided as any other.
pub(super) async fn taken(e: &Engine, row: &OutboxRow, parent: &str, name: &str, ours: Ours<'_>) -> Result<Taken, Fail> {
    let holder = match e.drive().child(parent, name).await {
        Ok(holder) => holder,
        Err(DriveError::NotFound) => return Ok(Taken::Free),
        Err(err) => return Err(err.into()),
    };
    let own_item = matches!(ours, Ours::Item(id) if id == holder.id);
    let is_ours = match ours {
        Ours::Folder => holder.folder.is_some(),
        Ours::File(hash) => holder.file.is_some() && holder.quick_xor_hash() == Some(hash),
        Ours::Item(_) => own_item && holder.name.as_deref() == Some(name),
        Ours::Sent { size, mtime } => holder.file.is_some() && holder.size == Some(size) && holder.mtime() == mtime,
    };
    if name.starts_with(SWAP_PREFIX) {
        // Its own temporary name: a replay adopts what it made there.
        return Ok(if is_ours { Taken::Adopt(Box::new(holder)) } else { Taken::Temporary(format!("{name}-{}", row.seq)) });
    }
    // The live rows of the item that holds the name: all that is asked of them.
    let held_by = holder.id.clone();
    let rows = e.store().call(move |s| s.outbox_for_item(&held_by)).await?;
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
        && (rows.iter().any(|r| r.item_id.as_deref() == Some(holder.id.as_str())) || {
            let held_by = holder.id.clone();
            e.store().call(move |s| s.local_handle(&held_by)).await?.is_some()
        });
    if is_ours && !known_here {
        return Ok(Taken::Adopt(Box::new(holder)));
    }
    if holder.file.is_some() && holder.size == Some(0) {
        let id = holder.id.clone();
        let known = e.store().call(move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some())).await?;
        if !known {
            return Ok(Taken::Held);
        }
    }
    Ok(Taken::Copy)
}

/// [`Taken::Held`]: the row waits for the name, with the usual backoff.
pub(super) fn held(row: &OutboxRow) -> Outcome {
    tracing::info!("{}: its name in OneDrive is held by an unfinished upload (another device, or one abandoned); waiting", row.rel.display());
    Outcome::backoff(Reason::NameHeld)
}

/// The row goes to `swap` first (saved before it is sent, WR7).
pub(super) async fn temporary(e: &Engine, row: &OutboxRow, parent: &str, swap: &str) -> Result<Outcome, Fail> {
    let (seq, parent, swap) = (row.seq, parent.to_owned(), swap.to_owned());
    e.store().call(move |s| s.outbox_set_target(seq, Some(&parent), Some(&swap))).await?;
    Ok(Outcome::again())
}

/// What a `409` leaves of a row's step ([`name_taken`]).
pub(in crate::upload) enum Named {
    /// What holds the name is what the row makes: the row's own commit
    /// follows, with this item.
    Adopt(Box<DriveItem>),
    /// Decided here: the row's outcome.
    Settled(Outcome),
}

/// A `409` for the row that takes (`parent`, `name`), carried out the same
/// way for every kind of row ([`taken`] says what holds the name): a name
/// free again is tried again; one held for now goes through a temporary
/// name, or waits for the unfinished upload that holds it; anything else is
/// kept beside the local object `found`, as a copy — with no object here to
/// keep, the row looks again later. Only what the row itself makes comes
/// back to the caller, to be adopted its own way.
pub(in crate::upload) async fn name_taken(
    e: &Arc<Engine>,
    disk: &Arc<Disk>,
    row: &OutboxRow,
    found: Option<&Found>,
    parent: &str,
    name: &str,
    ours: Ours<'_>,
) -> Result<Named, Fail> {
    Ok(match taken(e, row, parent, name, ours).await? {
        Taken::Free => Named::Settled(Outcome::again()),
        Taken::Temporary(swap) => Named::Settled(temporary(e, row, parent, &swap).await?),
        Taken::Adopt(item) => Named::Adopt(item),
        Taken::Held => Named::Settled(held(row)),
        Taken::Copy => Named::Settled(match found {
            Some(found) => copy(e, disk, row, found, parent, None).await?,
            None => Outcome::later(Reason::NotFound, RECHECK),
        }),
    })
}

/// §6's copy: the local object renamed beside what OneDrive holds at its
/// name (`<stem>-<machine><.ext>`, never over anything), konedrive's
/// attributes taken off, recorded as a conflict of kind `copy`. The row
/// becomes the copy's create (or mkdir) — or, for a move, the move to the
/// copy's name. `forget` is the item the copy was made from: its name is
/// placed again from the cloud, never deleted there.
///
/// The rename, the strip and the record of them (`outbox_copied`) are one
/// section: a stop of the worker never leaves a copy the outbox does not
/// know of, as it could not when they ran in one go on the row's task.
pub(in crate::upload) async fn copy(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, found: &Found, parent: &str, forget: Option<&str>) -> Result<Outcome, Fail> {
    let (event, copy_rel) = {
        let tree = tree(e).await;
        let moving = row.kind == OutboxKind::Move;
        let (engine, on, object) = (Arc::clone(e), Arc::clone(disk), found.clone());
        let (seq, parent, forget) = (row.seq, parent.to_owned(), forget.map(str::to_owned));
        #[cfg(test)]
        let (_row_alive, row_dropped) = std::sync::mpsc::channel::<()>();
        let (event, copy_rel) = blocking_under(Arc::clone(&tree), move || {
            let copy_name = local::rename_to_copy(&on, &object, engine.machine_name())?;
            let copy_rel = object.rel.with_file_name(&copy_name);
            if !moving {
                if let Some(copied) = local::find(&on, &copy_rel)? {
                    local::strip_found(&copied)?;
                }
            }
            let original = engine.root().path.join(&object.rel).display().to_string();
            let copy_path = engine.root().path.join(&copy_rel).display().to_string();
            let event = engine.event(kind::CONFLICT, &object.rel, copy_path.clone());
            let (inode, is_dir, rel, name) = (object.inode.clone(), object.is_dir, copy_rel.clone(), copy_name);
            let amend = move |next: &mut OutboxRow| {
                next.rel = rel;
                next.inode = Some(inode);
                next.target_parent = Some(parent);
                next.target_name = Some(name);
                next.state = OutboxState::Running;
                next.reset_for_resend();
                if !moving {
                    next.kind = if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create };
                    next.item_id = None;
                    next.base = None;
                }
            };
            #[cfg(test)]
            engine.before_record(row_dropped);
            let stored = event.clone();
            let recorded = engine.store().call_blocking(move |s| {
                let copied = ConflictCopy { forget: forget.as_deref(), at: now(), original: &original, copy: &copy_path };
                s.outbox_copied(seq, amend, &copied, Some(&stored))
            });
            Ok(recorded.map(|()| (event, copy_rel)))
        })
        .await??;
        if found.is_dir {
            let rebase = [OutboxOp::Rebase { from: found.rel.clone(), to: copy_rel.clone() }];
            e.store().call(move |s| s.outbox_apply(&rebase, now())).await?;
        }
        (event, copy_rel)
    };
    if let Some(url) = &row.session_url {
        cancel_session(e, url).await?;
    }
    tracing::info!("{} was changed in OneDrive too: the local version is kept as {}", found.rel.display(), copy_rel.display());
    e.host().activity(&event);
    e.host().cycle_wanted();
    Ok(Outcome::again())
}

/// Cancels an upload session given up (issue #47): the content changed, the
/// file went, the row became something else — its empty placeholder holds
/// the name in OneDrive until then. Cancelled, or gone already, it leaves the
/// list of sessions; a cancel that fails keeps it there, and a later run
/// cancels it ([`Engine::cancel_given_up`]), once no row points at it. Whether
/// it was cancelled.
pub(in crate::upload) async fn cancel_session(e: &Engine, url: &SessionUrl) -> Result<bool, Fail> {
    Ok(crate::upload::cancel_session(e.store(), e.drive(), url).await?)
}

/// Rename × rename (§6): the first to reach OneDrive wins, so the local
/// object goes where OneDrive has it. `None` where it cannot or must not:
/// OneDrive's folder is not placed here, the name is taken here, or it is a
/// name no listing places — the daemon's own `.konedrive-*` (a temporary
/// step of this very row, I1), a name OneDrive keeps but Linux cannot, or
/// one OneDrive refuses. The local place then stands. The caller holds the
/// tree lock, `tree`.
pub(in crate::upload) async fn follow_cloud(e: &Engine, disk: &Arc<Disk>, tree: &Tree, found: &Found, remote: &DriveItem) -> Result<Option<PathBuf>, Fail> {
    let (Some(parent), name) = place(remote) else { return Ok(None) };
    let placeable = matches!(classify(remote), Change::Upsert(row) if row.placement == Placement::Placed);
    if !placeable || name.starts_with(RESERVED_PREFIX) || names::refused(OsStr::new(&name)).is_some() {
        return Ok(None);
    }
    let Some(dir_rel) = e.store().call(move |s| s.locate(Table::Items, &parent)).await?.filter(|l| l.placed).map(|l| l.rel) else {
        return Ok(None);
    };
    let to_rel = dir_rel.join(&name);
    if to_rel == found.rel {
        return Ok(Some(to_rel));
    }
    let (on, object) = (Arc::clone(disk), found.clone());
    let renamed = blocking_under(Arc::clone(tree), move || {
        local::assert_under_lock();
        Ok(on.dir(&dir_rel).and_then(|to| on.rename(&object.dir, &object.name, &to, OsStr::new(&name))))
    })
    .await?;
    match renamed {
        Ok(()) => {
            if found.is_dir {
                let rebase = [OutboxOp::Rebase { from: found.rel.clone(), to: to_rel.clone() }];
                e.store().call(move |s| s.outbox_apply(&rebase, now())).await?;
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
///
/// The strip and its record (`outbox_orphan`) are one section, as in [`copy`].
pub(in crate::upload) async fn upload_as_new(e: &Arc<Engine>, row: &OutboxRow, found: &Found, parent: &str, id: &str) -> Result<Outcome, Fail> {
    let tree = tree(e).await;
    let is_dir = found.is_dir;
    let (inode, rel, parent, name) = (found.inode.clone(), found.rel.clone(), parent.to_owned(), found.name.to_str().map(str::to_owned));
    let amend = move |next: &mut OutboxRow| {
        next.kind = if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create };
        next.item_id = None;
        next.inode = Some(inode);
        next.rel = rel;
        next.base = None;
        next.target_parent = Some(parent);
        next.target_name = name;
        next.reset_for_resend();
    };
    let (seq, id) = (row.seq, id.to_owned());
    let (engine, object) = (Arc::clone(e), found.clone());
    #[cfg(test)]
    let (_row_alive, row_dropped) = std::sync::mpsc::channel::<()>();
    let event = blocking_under(Arc::clone(&tree), move || {
        local::strip_found(&object)?;
        let event = engine.event(kind::RESTORED, &object.rel, "deleted in OneDrive while it was changed here: uploaded again");
        #[cfg(test)]
        engine.before_record(row_dropped);
        let stored = event.clone();
        let recorded = engine.store().call_blocking(move |s| s.outbox_orphan(&id, seq, amend, Some(&stored)));
        Ok(recorded.map(|()| event))
    })
    .await??;
    e.host().activity(&event);
    e.host().cycle_wanted();
    Ok(Outcome::again())
}

/// A `create` or `mkdir` whose local object is under none of its names
/// (issue #27). A row is bound to its object, not to its name, and such an
/// object does not come back: a file saved over by replacing it is a new
/// object, and one moved where no row looked is found by the examination
/// and queued again as new. So the row ends now, with no retry: the upload
/// session it opened is cancelled, and it leaves the outbox with the rows
/// behind it of the same object that never got an item id — nothing of it
/// reached OneDrive. Except where it may have: see [`landed_away`].
pub(in crate::upload) async fn never_uploaded(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<Outcome, Fail> {
    let behind: Vec<i64> = match &row.inode {
        Some(inode) => {
            let inode = inode.clone();
            e.store().call(move |s| s.outbox_for_inode(&inode)).await?
        }.into_iter().filter(|r| r.seq > row.seq).map(|r| r.seq).collect(),
        None => Vec::new(),
    };
    if let Some(url) = &row.session_url {
        cancel_session(e, url).await?;
    }
    let detail = if landed_away(e, disk, row).await? {
        "removed here before its upload finished; what reached OneDrive went to its recycle bin"
    } else {
        "removed here before its upload finished"
    };
    tracing::info!("{} is not uploaded: {detail}", row.rel.display());
    let event = e.event(kind::NOT_UPLOADED, &row.rel, detail);
    let (seq, stored) = (row.seq, event.clone());
    e.store().call(move |s| s.outbox_drop_unsent(seq, &behind, Some(&stored))).await?;
    e.host().activity(&event);
    Ok(Outcome::Done)
}

/// A file's upload whose last request may have gone out with its answer
/// lost — the last fragment of a session, or the one request of a file up
/// to [`Limits::small_max`](crate::upload::Limits::small_max) — may have made the
/// item although the row never committed. Only such a row looks the name up
/// in the parent; the item there is this row's the way a replay's `409`
/// decides it ([`taken`]), by the size and time sent, the file being gone.
/// If it is, it goes to OneDrive's recycle bin. Whether it did.
async fn landed_away(e: &Engine, disk: &Arc<Disk>, row: &OutboxRow) -> Result<bool, Fail> {
    if row.kind != OutboxKind::Create {
        return Ok(false);
    }
    let Some((size, mtime)) = row.snapshot_sent() else { return Ok(false) };
    let limits = e.limits();
    let last_sent = size <= limits.small_max || (row.session_url.is_some() && row.session_next.unwrap_or(0).saturating_add(limits.chunk) >= size);
    if !last_sent {
        return Ok(false);
    }
    let Ok(local) = local_name(row) else { return Ok(false) };
    // The parent gone here as well, with no id recorded: its own delete
    // takes whatever is inside it in OneDrive.
    let Some(parent) = parent_of(e, disk, row).await? else { return Ok(false) };
    let name = wanted_name(row, &local);
    let Taken::Adopt(item) = taken(e, row, &parent, &name, Ours::Sent { size, mtime }).await? else { return Ok(false) };
    match e.drive().delete_item(&item.id, Guard::of_item(&item).as_str()).await {
        Ok(()) | Err(WriteError::NotFound) => Ok(true),
        // Changed there since: someone's now, not this row's.
        Err(WriteError::Changed) => Ok(false),
        Err(err) => Err(err.into()),
    }
}
