use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State};
use crate::upload::engine::{Engine, Fail, Outcome};
use crate::upload::Fault;
use crate::folder::disk::Disk;
use crate::helper::HelperError;
use crate::local::liveness::handles_current_async;
use crate::local::RECHECK;
use crate::folder::locks::{InodeGuard, InodeKey};
use crate::upload::steps::{blocking, blocking_under};
use konedrive_tree::outbox::{OutboxRow, Reason};
use konedrive_tree::{Kind, Placement, Table};

use super::place::{in_another_folder, parent_has, Place, proc_path, reopen_parent, verified_path};
use super::tidy::{remove, remove_at, remove_empty_dir, remove_info};
use super::trash::TrashEntry;
use super::walk::{dir_below, Met, open_met, reopen_dir, strip, walk};
use super::{absent, before_marker, CONTENT_LOCAL, last_place, Local, marker, MoveOuts, place, proved_path, state_of, superseded, TRASHED};

/// A file moved anywhere but the Trash: downloaded, stripped, then its item deleted (WR5).
pub(super) async fn elsewhere_file(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, id: &str, object: Arc<File>, shown: &Path) -> Result<Outcome, Fail> {
    if let Local::No(outcome) = e.make_local(&object, shown).await? {
        return Ok(outcome);
    }
    if let Some(outcome) = before_marker(e, disk, row, id, &object).await? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(CONTENT_LOCAL)).await?;
    blocking(move || strip(&object)).await?;
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", row.rel.display(), shown.display());
    finish(e, row).await
}

/// What the walk met at `m` below `top`, opened again off the runtime ([`open_met`]).
async fn reopened(top: &Arc<File>, m: &Met) -> Result<Arc<File>, Fail> {
    let (top, m) = (Arc::clone(top), m.clone());
    Ok(Arc::new(blocking(move || open_met(&top, &m)).await?))
}

/// A folder moved anywhere but the Trash: every placeholder of its item downloaded where it is,
/// the attributes taken off and every directory unmarked, then the folder deleted in OneDrive —
/// as a folder delete is: one unguarded `DELETE` of the folder itself, whatever it holds there by
/// then (F82 (10)).
pub(super) async fn elsewhere_folder(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, id: &str, object: Arc<File>, shown: &Path) -> Result<Outcome, Fail> {
    let (at, by) = (shown.to_owned(), Arc::clone(&object));
    let Some(top) = blocking(move || reopen_dir(&at, &by)).await?.map(Arc::new) else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
    if let Err(err) = e.moved_out().helper.mark_dir(&object).await {
        tracing::debug!("{} is not marked again yet: {err}", shown.display());
    }
    let inside = inside_of(e, id).await?;
    let below = Arc::clone(&top);
    let met = blocking(move || walk(&below)).await?;
    let mut ours: Vec<Met> = Vec::new();
    // Directories holding another item's placeholder (with a row of its own) stay marked.
    let mut keep_marked: HashSet<PathBuf> = HashSet::new();
    let mut found: HashSet<String> = HashSet::new();
    for m in met.iter().filter(|m| !m.is_dir) {
        let Some(item) = &m.id else { continue };
        if inside.contains(item) {
            let file = reopened(&top, m).await?;
            if let Local::No(outcome) = e.make_local(&file, &shown.join(&m.rel)).await? {
                return Ok(outcome);
            }
            found.insert(item.clone());
            ours.push(m.clone());
        } else {
            let (below, met) = (Arc::clone(&top), m.clone());
            let whole = blocking(move || Ok(matches!(placeholder::read_state(&open_met(&below, &met)?), Ok(Some(State::Hydrated))))).await?;
            if !whole {
                keep_marked.insert(m.dir().to_path_buf());
            }
        }
    }
    let extra = match left_since(e, disk, row, id, &inside, &found, Some(shown)).await? {
        Ok(extra) => extra,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(outcome) = before_marker(e, disk, row, id, &object).await? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(CONTENT_LOCAL)).await?;
    for (n, m) in ours.into_iter().enumerate() {
        let top = Arc::clone(&top);
        blocking(move || strip(&open_met(&top, &m)?)).await?;
        if n == 0 {
            e.fault(Fault::MidStrip)?;
        }
    }
    blocking(move || {
        for file in &extra {
            strip(file)?;
        }
        Ok(())
    })
    .await?;
    // Bottom up, the folder itself last.
    let dirs: Vec<Met> = met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))).cloned().collect();
    for m in dirs.iter().map(Some).chain(std::iter::once(None)) {
        let dir = match m {
            Some(m) => reopened(&top, m).await?,
            None => Arc::clone(&top),
        };
        if !keep_marked.contains(m.map_or(Path::new(""), |m| m.rel.as_path())) {
            e.unmark(disk, &dir).await;
        }
        blocking(move || strip(&dir)).await?;
    }
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", row.rel.display(), shown.display());
    finish(e, row).await
}

/// A file moved to the Trash: nothing is downloaded (Windows does the same). Downloaded content
/// stays there as the user's own file; a placeholder, which holds nothing, is removed with its
/// `.trashinfo`, and only once it has no link left does the item go to OneDrive's recycle bin.
pub(super) async fn trashed_file(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, id: &str, object: Arc<File>, entry: &TrashEntry) -> Result<Outcome, Fail> {
    let Some(path) = proved_path(&object).await? else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
    let key = InodeKey::of(&object)?;
    let Some(inode) = e.cfg.locks.try_lock(key) else { return Ok(Outcome::later(Reason::NotLocal, RECHECK)) };
    if let Some(outcome) = before_marker(e, disk, row, id, &object).await? {
        return Ok(outcome);
    }
    match state_of(&object).await? {
        Ok(Some(State::Hydrated)) => {
            e.set_marker(row, Some(CONTENT_LOCAL)).await?;
            let stripped = Arc::clone(&object);
            blocking_under(inode.hold(), move || strip(&stripped)).await?;
        }
        // Holds nothing whole: the cloud keeps it, in its recycle bin.
        Ok(Some(State::OnlineOnly | State::Hydrating)) => {
            e.set_marker(row, Some(TRASHED)).await?;
            let (entry, removed) = (entry.clone(), Arc::clone(&object));
            blocking_under(inode.hold(), move || remove(&removed, &path, Some(&entry))).await?;
            // Proved gone: no link left. Renamed meanwhile, or linked elsewhere, it is found where
            // it is at the next run.
            if object.metadata()?.nlink() != 0 {
                e.set_marker(row, None).await?;
                return Ok(Outcome::backoff(Reason::PlaceUnknown));
            }
        }
        _ => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
    }
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", row.rel.display());
    finish(e, row).await
}

/// A folder moved to the Trash: nothing is downloaded into it. Its downloaded files stay, as the
/// user's own; its placeholders go (each proved gone), and so do its directories left empty, and
/// the whole entry with its `.trashinfo` when nothing is left. A placeholder with another link is
/// downloaded instead.
pub(super) async fn trashed_folder(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, id: &str, object: Arc<File>, entry: &TrashEntry) -> Result<Outcome, Fail> {
    let by = Arc::clone(&object);
    let placed = blocking(move || {
        let Some(path) = verified_path(&by) else { return Ok(None) };
        Ok(reopen_dir(&path, &by)?.map(|top| (path, Arc::new(top))))
    })
    .await?;
    let Some((path, top)) = placed else { return Ok(Outcome::backoff(Reason::PlaceUnknown)) };
    let inside = inside_of(e, id).await?;
    let below = Arc::clone(&top);
    let met = blocking(move || walk(&below)).await?;
    let mut found: HashSet<String> = HashSet::new();
    // Held until the placeholders are gone: no fill starts meanwhile.
    let mut guards: Vec<InodeGuard> = Vec::new();
    // Each file of the item, and whether it stays (downloaded) or goes (a placeholder).
    let mut files: Vec<(Met, bool)> = Vec::new();
    for m in met.iter().filter(|m| !m.is_dir) {
        let Some(item) = m.id.as_ref().filter(|i| inside.contains(*i)) else { continue };
        let file = reopened(&top, m).await?;
        if file.metadata()?.nlink() > 1 {
            if let Local::No(outcome) = e.make_local(&file, &path.join(&m.rel)).await? {
                return Ok(outcome);
            }
        }
        let Some(guard) = e.cfg.locks.try_lock(InodeKey::of(&file)?) else { return Ok(Outcome::later(Reason::NotLocal, RECHECK)) };
        let stays = match state_of(&file).await? {
            Ok(Some(State::Hydrated)) => true,
            Ok(Some(State::OnlineOnly | State::Hydrating)) => false,
            _ => return Ok(Outcome::later(Reason::NotLocal, RECHECK)),
        };
        guards.push(guard);
        found.insert(item.clone());
        files.push((m.clone(), stays));
    }
    let extra = match left_since(e, disk, row, id, &inside, &found, Some(&path)).await? {
        Ok(extra) => extra,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(outcome) = before_marker(e, disk, row, id, &object).await? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(TRASHED)).await?;
    // The placeholders go first, each proved gone; nothing is stripped until they all are, so
    // that the marker can be taken off again with nothing stripped.
    let removed_all = {
        let (top, files) = (Arc::clone(&top), files.clone());
        let holds: Vec<_> = guards.iter().map(InodeGuard::hold).collect();
        blocking_under(holds, move || {
            let mut removed_all = true;
            for (m, _) in files.iter().filter(|(_, stays)| !stays) {
                let file = open_met(&top, m)?;
                if let Some(name) = m.rel.file_name() {
                    removed_all &= remove_at(&file, &dir_below(&top, m.dir())?, name)? && file.metadata()?.nlink() == 0;
                }
            }
            Ok(removed_all)
        })
        .await?
    };
    drop(guards);
    if !removed_all {
        // A placeholder renamed or linked meanwhile: nothing goes until it is found again.
        e.set_marker(row, None).await?;
        return Ok(Outcome::backoff(Reason::PlaceUnknown));
    }
    {
        let top = Arc::clone(&top);
        blocking(move || {
            for file in &extra {
                strip(file)?;
            }
            for (m, _) in files.iter().filter(|(_, stays)| *stays) {
                strip(&open_met(&top, m)?)?;
            }
            Ok(())
        })
        .await?;
    }
    // Bottom up: each directory of the item unmarked, stripped, and removed if left empty.
    for m in met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
        let dir = reopened(&top, m).await?;
        e.unmark(disk, &dir).await;
        let (below, in_dir) = (Arc::clone(&top), m.dir().to_owned());
        let name = m.rel.file_name().map(OsStr::to_os_string);
        blocking(move || {
            let parent = dir_below(&below, &in_dir)?;
            strip(&dir)?;
            if let Some(name) = name {
                remove_empty_dir(&dir, &parent, &name);
            }
            Ok(())
        })
        .await?;
    }
    e.unmark(disk, &top).await;
    let entry = entry.clone();
    blocking(move || {
        strip(&top)?;
        // The whole entry went: its `.trashinfo` goes too.
        if path == entry.top && std::fs::read_dir(proc_path(&top))?.next().is_none() {
            if let (Some(parent), Some(name)) = (path.parent().and_then(|p| reopen_parent(p).ok()), path.file_name()) {
                remove_empty_dir(&top, &parent, name);
                if !parent_has(&parent, name) {
                    remove_info(&entry);
                }
            }
        }
        Ok(())
    })
    .await?;
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", row.rel.display());
    finish(e, row).await
}

/// The ids the base has inside folder `id` now, the folder's own included.
async fn inside_of(e: &Engine, id: &str) -> Result<HashSet<String>, Fail> {
    let folder = id.to_owned();
    let mut inside: HashSet<String> = e.store().call(move |s| s.descendants(Table::Items, &folder)).await?.into_iter().collect();
    inside.insert(id.to_owned());
    Ok(inside)
}

/// What the base still has inside folder `id` that was placed here and is not among what the
/// walk found (`found`): each is asked after by its own handle. Gone is fine (deleted by the
/// user) when the handles are this filesystem's; so is `EPERM` once the row is marked (it took
/// that file's attributes off itself); a file alive outside the folder left the moved-out folder
/// since, and is made local where it is, like the folder's own (returned, to be stripped with
/// them); anything else — alive in the folder, unreachable, unanswered, with no handle — keeps
/// the folder in OneDrive for now.
async fn left_since(
    e: &Arc<Engine>,
    disk: &Arc<Disk>,
    row: &OutboxRow,
    id: &str,
    inside: &HashSet<String>,
    found: &HashSet<String>,
    top: Option<&Path>,
) -> Result<Result<Vec<Arc<File>>, Outcome>, Fail> {
    let mo = e.moved_out();
    let root = disk.dir(Path::new(""))?;
    let asked = id.to_owned();
    let folder = e.store().call(move |s| s.locate(Table::Items, &asked)).await?.map(|l| l.rel);
    let mut extra = Vec::new();
    for item in inside.iter().filter(|i| i.as_str() != id && !found.contains(*i)) {
        let asked = item.clone();
        let Some(base) = e.store().call(move |s| s.get(Table::Items, &asked)).await? else { continue };
        if base.kind != Kind::File || base.placement != Placement::Placed {
            continue;
        }
        let asked = item.clone();
        let Some(handle) = e.store().call(move |s| s.local_handle(&asked)).await? else {
            return Ok(Err(Outcome::backoff(Reason::Unreachable(None))));
        };
        let object = match mo.helper.open_by_handle(&root, &handle).await {
            Ok(object) => Arc::new(File::from(object)),
            Err(HelperError::Refused(libc::ESTALE)) if !handles_current_async(e.store(), &root).await => {
                return Ok(Err(Outcome::backoff(Reason::StaleHandle)));
            }
            // Gone with its evidence: nothing, or another object, at its place in the folder
            // where the folder is now.
            Err(HelperError::Refused(libc::ESTALE)) => {
                let asked = item.clone();
                let at = e.store().call(move |s| s.locate(Table::Items, &asked)).await?.map(|l| l.rel);
                let there = match (top, folder.as_deref(), at.as_deref()) {
                    (Some(top), Some(folder), Some(at)) => at.strip_prefix(folder).ok().map(|inside| top.join(inside)),
                    _ => None,
                };
                if absent(there.as_deref(), &handle).await? {
                    continue;
                }
                return Ok(Err(Outcome::backoff(Reason::GoneUnproved)));
            }
            Err(HelperError::Refused(libc::EPERM)) if marker(row) => continue,
            Err(HelperError::Refused(_)) => return Ok(Err(Outcome::backoff(Reason::Unreachable(None)))),
            Err(_) => return Ok(Err(Outcome::backoff(Reason::NoHelper))),
        };
        let shown = match place(e, disk, &object, &handle).await? {
            Place::Elsewhere(Some(path)) => path,
            Place::Trash(entry) => entry.top,
            Place::Elsewhere(None) => return Ok(Err(Outcome::backoff(Reason::PlaceUnknown))),
            Place::Inside | Place::Unknown => return Ok(Err(Outcome::backoff(Reason::BackInside))),
        };
        if let Local::No(outcome) = e.make_local(&object, &shown).await? {
            return Ok(Err(outcome));
        }
        extra.push(object);
    }
    Ok(Ok(extra))
}

/// The item leaves OneDrive, as a delete does (§4.7): `If-Match`, `404` done — a folder, unguarded,
/// whole, whatever changed inside it since.
pub(super) async fn finish(e: &Arc<Engine>, row: &OutboxRow) -> Result<Outcome, Fail> {
    crate::upload::steps::delete(e, row.clone()).await
}

/// The object is gone (`ESTALE`, twice, on this filesystem's handles): the user deleted it after
/// it left (§5), and its item is deleted as any delete is — a folder only once what left it since
/// is local where it went, or gone too.
pub(super) async fn gone(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    let asked = id.to_owned();
    let folder = e.store().call(move |s| s.get(Table::Items, &asked)).await?.is_some_and(|item| item.kind == Kind::Folder);
    if folder {
        let inside = inside_of(e, id).await?;
        let extra = match left_since(e, disk, row, id, &inside, &HashSet::new(), last_place(row)).await? {
            Ok(extra) => extra,
            Err(outcome) => return Ok(outcome),
        };
        if let Some(outcome) = superseded(e, row, id).await? {
            return Ok(outcome);
        }
        e.set_marker(row, Some(CONTENT_LOCAL)).await?;
        blocking(move || {
            for file in &extra {
                strip(file)?;
            }
            Ok(())
        })
        .await?;
    }
    finish(e, row).await
}

/// Whether the row's object still stands where it was last proved to be, inside another account's
/// folder: the name there has the row's handle.
pub(super) async fn stands_in_another_folder(mo: &MoveOuts, disk: &Arc<Disk>, row: &OutboxRow, handle: &FileHandle) -> Result<bool, Fail> {
    let Some(place) = last_place(row).map(Path::to_owned) else { return Ok(false) };
    let (mo, disk, handle) = (mo.clone(), Arc::clone(disk), handle.clone());
    blocking(move || {
        if !in_another_folder(&mo, &disk, &place) {
            return Ok(false);
        }
        let (Some(parent), Some(name)) = (place.parent(), place.file_name()) else { return Ok(false) };
        Ok(reopen_parent(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()).as_ref() == Some(&handle))
    })
    .await
}

/// The object is gone, and was last proved to be inside another account's folder (final review
/// I5), or stands there taken for that account's own (m5): that account may have removed it as
/// none of its own, or the user deleted it there. Nothing is deleted in OneDrive: the row goes, the
/// item and what is inside it forget their local objects, and the reconcile places them again in
/// this folder.
pub(super) async fn kept(e: &Arc<Engine>, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    let event = e.event(crate::upload::kind::RESTORED, &row.rel, "it was last in another account's folder, and stays in OneDrive");
    {
        let _tree = e.cfg.tree_lock.lock().await;
        let (seq, id, stored) = (row.seq, id.to_owned(), event.clone());
        e.store().call(move |s| s.outbox_drop(seq, None, Some(&id), Some(&stored))).await?;
    }
    tracing::info!("{} went from another account's folder: it stays in OneDrive, and comes back here", row.rel.display());
    e.cfg.host.activity(&event);
    e.cfg.host.full_cycle_wanted();
    Ok(Outcome::Done)
}
