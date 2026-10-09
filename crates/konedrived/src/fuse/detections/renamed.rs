//! A file or a directory a program renamed or moved inside the folder.
//!
//! An item is moved in OneDrive by the outbox. A local object is uploaded from its new
//! place, or no longer uploaded when the new place stays on this computer. A local file
//! renamed over an item's file is that item's new content: programs save that way.

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::TreeError;

use super::{create_entries, create_or_mkdir, delete_item, forget_object, move_item, rebase, update_item, wait_for_writer};
use super::{FolderEvent, LocalEntry, LocalObject, OutboxBatch, FolderPlace, StoreFacts};

/// A rename that was made in the folder.
#[derive(Debug)]
pub(in crate::fuse) enum Renamed {
    /// A local file was renamed over the file of the item `over`, to the place `to`: the
    /// file is that item's new content, and what the outbox held of the file itself goes —
    /// it is never sent under its own name. `open`: a program still writes it.
    SavedOver { file: LocalObject, open: bool, over: String, to: FolderPlace },
    /// Anything else: `entry` moved from `from` to `to`, where `displaced` stood.
    Moved { entry: MovedEntry, from: PathBuf, to: FolderPlace, displaced: Option<DisplacedEntry> },
}

/// What moved.
#[derive(Debug)]
pub(in crate::fuse) enum MovedEntry {
    /// An item, a folder when `dir`: moved in OneDrive. `object` is what the backing
    /// directory holds of it, now at the new place: its downloaded copy, or its directory.
    Item { id: String, dir: bool, object: Option<FileHandle> },
    /// A local file. `uploaded`: the new place is one things are uploaded from — else the
    /// file stays on this computer from now on. `open`: a program holds it open for writing.
    LocalFile { file: LocalObject, open: bool, uploaded: bool },
    /// A local directory. `uploaded` is what of it is uploaded from the new place: itself
    /// first, then all it holds. Nothing, when it stays on this computer from now on.
    LocalDir { handle: FileHandle, uploaded: Vec<LocalEntry> },
}

/// What stood at the new place, and went.
#[derive(Debug)]
pub(in crate::fuse) enum DisplacedEntry {
    /// An item: removed from OneDrive.
    Item(String),
    /// A local file, or an empty local directory.
    LocalObject(FileHandle),
}

impl FolderEvent for Renamed {
    fn detect(self, facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        match self {
            Renamed::SavedOver { file, open, over, to } => save_over(facts, &file, open, &over, &to, now),
            Renamed::Moved { entry, from, to, displaced } => {
                let at = to.rel();
                // What stood at the new place first: it frees the name.
                let mut batch = OutboxBatch::of(match &displaced {
                    Some(DisplacedEntry::Item(id)) => delete_item(facts, id, &at)?,
                    Some(DisplacedEntry::LocalObject(handle)) => forget_object(facts, handle, &at)?,
                    None => Vec::new(),
                });
                let dir = match entry {
                    MovedEntry::Item { id, dir, object } => {
                        batch.detections.extend(move_base_item(facts, &id, object, &to)?);
                        dir
                    }
                    MovedEntry::LocalFile { file, open, uploaded: true } => {
                        batch.detections.push(upload_from(facts, &file, open, &to, now)?);
                        false
                    }
                    MovedEntry::LocalFile { file, uploaded: false, .. } => {
                        keep_local(facts, &file.handle, &from, &at, &mut batch)?;
                        false
                    }
                    MovedEntry::LocalDir { handle, uploaded } if uploaded.is_empty() => {
                        keep_local(facts, &handle, &from, &at, &mut batch)?;
                        true
                    }
                    MovedEntry::LocalDir { uploaded, .. } => {
                        batch.detections.extend(create_entries(&uploaded, now));
                        true
                    }
                };
                // A directory takes the rows of what is below it along.
                if dir {
                    batch.ops.push(rebase(&from, &at));
                }
                Ok(batch)
            }
        }
    }
}

/// Save by rename. Nothing when the base no longer has the item.
fn save_over(facts: &impl StoreFacts, file: &LocalObject, open: bool, over: &str, to: &FolderPlace, now: i64) -> Result<OutboxBatch, TreeError> {
    let Some(base) = facts.base_item(over)? else { return Ok(OutboxBatch::default()) };
    let pending = facts.outbox_rows_of(&file.handle)?;
    let ops = pending.iter().filter(|row| row.state != OutboxState::Running).map(|row| OutboxOp::Remove(row.seq)).collect();
    let mut update = update_item(to, &base, file);
    if open {
        wait_for_writer(&mut update, now);
    }
    Ok(OutboxBatch { detections: vec![update], ops })
}

/// The item `id` stands at `to` now. Its content is the base's unless the outbox holds new
/// content of it. Nothing when the base no longer has the item.
fn move_base_item(facts: &impl StoreFacts, id: &str, object: Option<FileHandle>, to: &FolderPlace) -> Result<Option<Detection>, TreeError> {
    let Some(base) = facts.base_item(id)? else { return Ok(None) };
    let mut moved = move_item(to, &base, object);
    moved.same_content = !facts.has_update(id)?;
    Ok(Some(moved))
}

/// A local file is uploaded from `to`. One that is going up right now goes up as it is:
/// only where it stands changed.
fn upload_from(facts: &impl StoreFacts, file: &LocalObject, open: bool, to: &FolderPlace, now: i64) -> Result<Detection, TreeError> {
    let mut d = create_or_mkdir(to, file, open, now);
    if facts.outbox_rows_of(&file.handle)?.iter().any(|row| row.state == OutboxState::Running) {
        d.kind = OutboxKind::Move;
    }
    Ok(d)
}

/// A local object stays on this computer from now on, with what is below it: what the
/// outbox held of it goes, and so does every row of a local object at and below the place it
/// came from that is not being sent.
fn keep_local(facts: &impl StoreFacts, handle: &FileHandle, from: &Path, at: &Path, batch: &mut OutboxBatch) -> Result<(), TreeError> {
    batch.detections.extend(forget_object(facts, handle, at)?);
    let below = facts.rows_at_or_under(from)?;
    let unsent = below.iter().filter(|row| row.item_id.is_none() && row.state != OutboxState::Running);
    batch.ops.extend(unsent.map(|row| OutboxOp::Remove(row.seq)));
    Ok(())
}

#[cfg(test)]
mod tests;
