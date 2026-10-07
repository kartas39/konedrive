//! The rows that send no content: `mkdir`, `move` and `delete`.

use konedrive_tree::ActivityKind;
use std::sync::Arc;

use konedrive_fs::placeholder::State;

use super::sections::{blocking, blocking_under, tree};
use super::shared::{answer_row, commit_row, follow_cloud, in_swap, local_name, locate, name_taken, never_uploaded, parent_of, place, upload_as_new, wanted_name, Guard, Named, Ours};
use crate::folder::disk::Disk;
use crate::local::RECHECK;
use crate::upload::engine::{Engine, Fail, Outcome};
use crate::upload::local::{self, Found};
use crate::upload::Fault;
use konedrive_graph::drive::{DriveError, DriveItem, ItemChange, WriteError};
use konedrive_tree::outbox::{Base, Committed, OutboxRow, Reason};
use konedrive_tree::{Kind, Table};

pub(super) async fn mkdir(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Result<Outcome, Fail> {
    let local = local_name(&row)?;
    let Some(found) = locate(e, disk, &row).await?.filter(|f| f.is_dir) else { return never_uploaded(e, disk, &row).await };
    let Some(parent) = parent_of(e, disk, &row).await? else { return Ok(Outcome::later(Reason::Parent, RECHECK)) };
    let name = wanted_name(&row, &local);
    // Opened before the request, as a file's content is: the commit marks the
    // directory that was made, wherever it is by then — renamed, or removed.
    let object = found.clone();
    let dir = blocking(move || object.open_dir()).await?;
    match e.drive().create_folder(&parent, &name).await {
        Ok(item) => {
            e.fault(Fault::AfterSend)?;
            commit_dir(e, &row, &found, dir, &item, &parent).await
        }
        Err(WriteError::NameExists) => match name_taken(e, disk, &row, Some(&found), &parent, &name, Ours::Folder).await? {
            // A folder of that name: adopted, and the contents merge file by
            // file (`docs/design/writes.md` §6.2).
            Named::Adopt(item) => commit_dir(e, &row, &found, dir, &item, &parent).await,
            Named::Settled(outcome) => Ok(outcome),
        },
        Err(WriteError::NotFound) => {
            e.host().cycle_wanted();
            Ok(Outcome::backoff(Reason::Parent))
        }
        Err(other) => Err(other.into()),
    }
}

/// A folder's commit, on the directory `dir` opened before the request. One
/// removed since is committed all the same, with its handle: the item exists
/// in OneDrive now, and only the base knowing it lets the examination delete
/// it there (a commit that failed here left it in OneDrive for good, and the
/// reconcile placed it back as new).
async fn commit_dir(e: &Engine, row: &OutboxRow, found: &Found, dir: std::fs::File, item: &DriveItem, parent: &str) -> Result<Outcome, Fail> {
    let answer = answer_row(item, Some(parent))?;
    let tree = tree(e).await;
    let id = item.id.clone();
    blocking_under(Arc::clone(&tree), move || local::commit_dir(&dir, &id)).await?;
    e.fault(Fault::AfterCommitStep1)?;
    let event = e.event(ActivityKind::Uploaded, &found.rel, "folder");
    commit_row(e, row, &answer, found.inode.handle.as_ref(), parent, event).await?;
    Ok(Outcome::Done)
}

pub(super) async fn moved(e: &Arc<Engine>, disk: &Arc<Disk>, row: OutboxRow) -> Result<Outcome, Fail> {
    let (Some(id), Some(base)) = (row.item_id.clone(), row.base.clone()) else { return Ok(Outcome::blocked(Reason::NoItem)) };
    let local = local_name(&row)?;
    let Some(parent) = super::shared::parent_recorded(e, disk, &row).await? else { return Ok(Outcome::later(Reason::Parent, RECHECK)) };
    let name = wanted_name(&row, &local);
    let found = locate(e, disk, &row).await?;
    let Some(guard) = Guard::of_base(&base) else { return Ok(Outcome::blocked(Reason::NoGuard)) };
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
            let remote = e.drive().item(&id).await?;
            return commit_move(e, &row, found.as_ref(), &remote, &parent).await;
        }
        // Where the base has it already: nothing to send.
        e.store().call(move |s| s.outbox_drop(row.seq, None, None)).await?;
        return Ok(Outcome::Done);
    }
    match e.drive().update_item(&id, guard.as_str(), &change).await {
        Ok(item) => {
            e.fault(Fault::AfterSend)?;
            commit_move(e, &row, found.as_ref(), &item, &parent).await
        }
        Err(WriteError::NameExists) => match name_taken(e, disk, &row, found.as_ref(), &parent, &name, Ours::Item(&id)).await? {
            Named::Adopt(item) => commit_move(e, &row, found.as_ref(), &item, &parent).await,
            Named::Settled(outcome) => Ok(outcome),
        },
        Err(WriteError::Changed) => {
            let remote = match e.drive().item(&id).await {
                Ok(remote) => remote,
                Err(DriveError::NotFound) => return move_gone(e, disk, &row, found.as_ref(), &id, &parent).await,
                Err(err) => return Err(err.into()),
            };
            let (remote_parent, remote_name) = place(&remote);
            if remote_parent.as_deref() == Some(parent.as_str()) && remote_name == name {
                // It went through before (§10: a PATCH sent, no answer).
                return commit_move(e, &row, found.as_ref(), &remote, &parent).await;
            }
            if remote_parent.as_deref() != base.parent.as_deref() || Some(remote_name.as_str()) != base.name.as_deref() {
                // Moved there as well: the first to reach OneDrive wins (§7).
                if let Some(found) = &found {
                    let tree = tree(e).await;
                    if let Some(to_rel) = follow_cloud(e, disk, &tree, found, &remote).await? {
                        let answer = answer_row(&remote, None)?;
                        let handle = found.inode.handle.clone();
                        let (on, at) = (Arc::clone(disk), to_rel.clone());
                        blocking_under(Arc::clone(&tree), move || {
                            local::mark(&on, &at, None);
                            Ok(())
                        })
                        .await?;
                        let event = e.event(ActivityKind::Moved, &to_rel, format!("renamed in OneDrive first; was {} here", found.rel.display()));
                        let (seq, stored) = (row.seq, event.clone());
                        e.store().call(move |s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: handle.as_ref() }, Some(&stored))).await?;
                        e.host().activity(&event);
                        return Ok(Outcome::Done);
                    }
                }
            }
            // Changed there, not moved (or its place cannot be followed here,
            // such as its own temporary name): the move goes again against
            // the fresh eTag; the delta brings the content (§7).
            // Where the folder cannot hold OneDrive's place, only what the
            // user changed is sent: a rename here is no move back.
            let held = super::shared::holds(e, &remote).await?;
            let fresh = super::shared::base_after_a_change(&base, &parent, &name, &remote, held);
            let seq = row.seq;
            e.store().call(move |s| s.outbox_amend(seq, |next| next.base = Some(fresh))).await?;
            Ok(Outcome::again())
        }
        Err(WriteError::NotFound) => move_gone(e, disk, &row, found.as_ref(), &id, &parent).await,
        Err(other) => Err(other.into()),
    }
}

async fn commit_move(e: &Engine, row: &OutboxRow, found: Option<&Found>, item: &DriveItem, parent: &str) -> Result<Outcome, Fail> {
    let answer = answer_row(item, Some(parent))?;
    let was = match &row.item_id {
        Some(id) => {
            let id = id.clone();
            e.store().call(move |s| s.locate(Table::Items, &id)).await?
        }.map(|l| l.rel.display().to_string()).unwrap_or_default(),
        None => String::new(),
    };
    let tree = tree(e).await;
    let handle = found.and_then(|f| f.inode.handle.clone()).or_else(|| row.inode.as_ref().and_then(|i| i.handle.clone()));
    let rel = found.map(|f| f.rel.as_path()).unwrap_or(&row.rel);
    if let Some(object) = found.cloned() {
        blocking_under(Arc::clone(&tree), move || {
            local::clear_mark(&object);
            Ok(())
        })
        .await?;
    }
    let event = e.event(ActivityKind::CloudMoved, rel, was);
    commit_row(e, row, &answer, handle.as_ref(), parent, event).await?;
    Ok(Outcome::Done)
}

/// A `404` for a move: gone from OneDrive meanwhile (§7, move/delete).
/// Content decides: a downloaded file, or a folder, is uploaded again as
/// new at its new place; a placeholder, which holds nothing here, follows
/// the delete.
async fn move_gone(e: &Arc<Engine>, disk: &Arc<Disk>, row: &OutboxRow, found: Option<&Found>, id: &str, parent: &str) -> Result<Outcome, Fail> {
    let Some(found) = found else {
        return gone(e, row, id, "deleted in OneDrive").await;
    };
    if found.is_dir {
        return upload_as_new(e, row, found, parent, id).await;
    }
    let object = found.clone();
    match blocking(move || object.state()).await? {
        Some(State::OnlineOnly) => {
            let tree = tree(e).await;
            // Still the same placeholder, holding nothing, and nobody has it
            // open (a write lease, as a free-up takes): removed.
            let (on, object) = (Arc::clone(disk), found.clone());
            if !blocking_under(Arc::clone(&tree), move || local::remove_placeholder(&on, &object)).await? {
                return Ok(Outcome::later(Reason::NotLocal, RECHECK));
            }
            let event = e.event(ActivityKind::CloudDeleted, &found.rel, "deleted in OneDrive; the placeholder here went too");
            let (seq, id, stored) = (row.seq, id.to_owned(), event.clone());
            e.store().call(move |s| s.outbox_commit(seq, Committed::Gone { item_id: &id }, Some(&stored))).await?;
            e.host().activity(&event);
            Ok(Outcome::Done)
        }
        Some(State::Hydrated) | None => upload_as_new(e, row, found, parent, id).await,
        Some(_) => Ok(Outcome::later(Reason::NotLocal, RECHECK)),
    }
}

pub(in crate::upload) async fn delete(e: &Arc<Engine>, row: OutboxRow) -> Result<Outcome, Fail> {
    let Some(id) = row.item_id.clone() else {
        // Never in OneDrive (its create never landed): nothing to delete.
        tracing::info!("{} was never uploaded: its delete leaves the outbox", row.rel.display());
        let seq = row.seq;
        e.store().call(move |s| s.outbox_drop(seq, None, None)).await?;
        return Ok(Outcome::Done);
    };
    let base = row.base.clone().unwrap_or_default();
    let asked = id.clone();
    let folder = e.store().call(move |s| s.get(Table::Items, &asked)).await?.is_some_and(|item| item.kind == Kind::Folder);
    if folder {
        return delete_folder(e, &row, &id).await;
    }
    let Some(guard) = Guard::of_base(&base) else { return Ok(Outcome::blocked(Reason::NoGuard)) };
    match e.drive().delete_item(&id, guard.as_str()).await {
        Ok(()) => {
            e.fault(Fault::AfterSend)?;
            gone(e, &row, &id, "to OneDrive's recycle bin").await
        }
        // Gone already (§10: a DELETE sent, no answer).
        Err(WriteError::NotFound) => gone(e, &row, &id, "to OneDrive's recycle bin").await,
        Err(WriteError::Changed) => file_changed(e, &row, &id, &base).await,
        Err(other) => Err(other.into()),
    }
}

/// Commit step 2 of a delete, under the tree lock.
async fn gone(e: &Engine, row: &OutboxRow, id: &str, why: &str) -> Result<Outcome, Fail> {
    let event = e.event(ActivityKind::CloudDeleted, &row.rel, why);
    let _tree = e.tree_lock().lock().await;
    let (seq, id, stored) = (row.seq, id.to_owned(), event.clone());
    e.store().call(move |s| s.outbox_commit(seq, Committed::Gone { item_id: &id }, Some(&stored))).await?;
    e.host().activity(&event);
    Ok(Outcome::Done)
}

/// OneDrive's version comes back: the delete is dropped, and the item is
/// placed again by the reconcile (§7, delete × edit: remote wins). Under the
/// tree lock, so that a cycle's swap cannot give the item its old local
/// object back.
async fn restored(e: &Engine, row: &OutboxRow, id: &str, why: &str) -> Result<Outcome, Fail> {
    let event = e.event(ActivityKind::Restored, &row.rel, why);
    {
        let _tree = e.tree_lock().lock().await;
        let (seq, id, stored) = (row.seq, id.to_owned(), event.clone());
        e.store().call(move |s| s.outbox_drop(seq, Some(&id), Some(&stored))).await?;
    }
    e.host().activity(&event);
    e.host().full_cycle_wanted();
    Ok(Outcome::Done)
}

/// `412` on a file's delete: only its name or place changed there — it is
/// the content the user deleted, and goes with the fresh eTag — or its
/// content changed, and OneDrive's version comes back (§7).
async fn file_changed(e: &Engine, row: &OutboxRow, id: &str, base: &Base) -> Result<Outcome, Fail> {
    for _ in 0..3 {
        let remote = match e.drive().item(id).await {
            Ok(remote) => remote,
            Err(DriveError::NotFound) => return gone(e, row, id, "to OneDrive's recycle bin").await,
            Err(err) => return Err(err.into()),
        };
        if remote.c_tag.is_none() || remote.c_tag != base.ctag {
            return restored(e, row, id, "changed in OneDrive after it was deleted here").await;
        }
        match e.drive().delete_item(id, Guard::of_item(&remote).as_str()).await {
            Ok(()) | Err(WriteError::NotFound) => return gone(e, row, id, "to OneDrive's recycle bin").await,
            Err(WriteError::Changed) => continue,
            Err(other) => return Err(other.into()),
        }
    }
    Ok(Outcome::backoff(Reason::ChangedAgain))
}

/// A folder's delete (§6.1): one `DELETE` of the whole folder, unguarded —
/// no `If-Match`, whatever changed inside it in OneDrive since. As on
/// Windows, the folder goes to the recycle bin whole; the recycle bin is the
/// safety net ([decisions.md](../../../../docs/design/decisions.md), "A
/// folder delete is the whole folder, as on Windows").
async fn delete_folder(e: &Engine, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    match e.drive().delete_folder(id).await {
        Ok(()) => {
            e.fault(Fault::AfterSend)?;
            gone(e, row, id, "to OneDrive's recycle bin").await
        }
        // Gone already (§10: a DELETE sent, no answer).
        Err(WriteError::NotFound) => gone(e, row, id, "to OneDrive's recycle bin").await,
        Err(other) => Err(other.into()),
    }
}
