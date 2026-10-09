//! A file or a directory a program renamed or moved inside the folder.
//!
//! What is in OneDrive is moved there by the outbox. A local-only entry is uploaded from its
//! new place, or no longer uploaded when the new place stays on this computer. A local-only
//! file renamed over a OneDrive file is that file's new content: programs save that way.

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::TreeError;

use super::{create_local_only_entries, create_or_mkdir, delete_in_onedrive, forget_local_only, move_in_onedrive, rebase, update_content, wait_for_writer};
use super::{FolderEvent, FolderPlace, LocalOnlyEntry, OutboxBatch, StoreFacts};
use crate::fuse::backing_directory::DiskEntry;

/// A rename that was made in the folder.
#[derive(Debug)]
pub(in crate::fuse) enum EntryRenamed {
    /// A local-only file was renamed over the OneDrive file `over`, to the place `to`: the
    /// file is that file's new content, and what the outbox held of the local-only file
    /// itself is removed — it is never sent under its own name. `open`: a program still
    /// writes it.
    SavedOverOneDriveFile { file: DiskEntry, open: bool, over: String, to: FolderPlace },
    /// Anything else: `entry` moved from `from` to `to`, where `displaced` was.
    Moved { entry: MovedEntry, from: PathBuf, to: FolderPlace, displaced: Option<DisplacedEntry> },
}

/// What moved.
#[derive(Debug)]
pub(in crate::fuse) enum MovedEntry {
    /// A file of OneDrive, or a directory when `directory`: moved in OneDrive. `handle` is
    /// of what the backing directory holds for it, now at the new place: its downloaded
    /// copy, or its directory.
    InOneDrive { id: String, directory: bool, handle: Option<FileHandle> },
    /// A local-only file. `uploaded`: the new place is one things are uploaded from — else
    /// the file stays on this computer from now on. `open`: a program holds it open for
    /// writing.
    LocalOnlyFile { file: DiskEntry, open: bool, uploaded: bool },
    /// A local-only directory. `uploaded` is what of it is uploaded from the new place:
    /// itself first, then all it holds. Nothing, when it stays on this computer from now on.
    LocalOnlyDirectory { handle: FileHandle, uploaded: Vec<LocalOnlyEntry> },
}

/// What was at the new place, and was replaced.
#[derive(Debug)]
pub(in crate::fuse) enum DisplacedEntry {
    /// A file or directory of OneDrive, by its id: removed from OneDrive.
    InOneDrive(String),
    /// A local-only file, or an empty local-only directory.
    LocalOnly(FileHandle),
}

impl FolderEvent for EntryRenamed {
    fn detect(self, facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        match self {
            EntryRenamed::SavedOverOneDriveFile { file, open, over, to } => save_over(facts, &file, open, &over, &to, now),
            EntryRenamed::Moved { entry, from, to, displaced } => {
                let at = to.rel();
                // What was at the new place first: it frees the name.
                let mut batch = OutboxBatch::of(match &displaced {
                    Some(DisplacedEntry::InOneDrive(id)) => delete_in_onedrive(facts, id, &at)?,
                    Some(DisplacedEntry::LocalOnly(handle)) => forget_local_only(facts, handle, &at)?,
                    None => Vec::new(),
                });
                let directory = match entry {
                    MovedEntry::InOneDrive { id, directory, handle } => {
                        batch.detections.extend(move_base_row(facts, &id, handle, &to)?);
                        directory
                    }
                    MovedEntry::LocalOnlyFile { file, open, uploaded: true } => {
                        batch.detections.push(upload_from(facts, &file, open, &to, now)?);
                        false
                    }
                    MovedEntry::LocalOnlyFile { file, uploaded: false, .. } => {
                        keep_off_onedrive(facts, &file.handle, &from, &at, &mut batch)?;
                        false
                    }
                    MovedEntry::LocalOnlyDirectory { handle, uploaded } if uploaded.is_empty() => {
                        keep_off_onedrive(facts, &handle, &from, &at, &mut batch)?;
                        true
                    }
                    MovedEntry::LocalOnlyDirectory { uploaded, .. } => {
                        batch.detections.extend(create_local_only_entries(&uploaded, now));
                        true
                    }
                };
                // A directory takes the rows of what is below it along.
                if directory {
                    batch.ops.push(rebase(&from, &at));
                }
                Ok(batch)
            }
        }
    }
}

/// Save by rename. Nothing when the base no longer has the file.
fn save_over(facts: &impl StoreFacts, file: &DiskEntry, open: bool, over: &str, to: &FolderPlace, now: i64) -> Result<OutboxBatch, TreeError> {
    let Some(base) = facts.base_row(over)? else { return Ok(OutboxBatch::default()) };
    let pending = facts.outbox_rows_of(&file.handle)?;
    let ops = pending.iter().filter(|row| row.state != OutboxState::Running).map(|row| OutboxOp::Remove(row.seq)).collect();
    let mut update = update_content(to, &base, file);
    if open {
        wait_for_writer(&mut update, now);
    }
    Ok(OutboxBatch { detections: vec![update], ops })
}

/// The file or directory of OneDrive `id` is at `to` now. Its content is the base's unless
/// the outbox holds new content of it. Nothing when the base no longer has it.
fn move_base_row(facts: &impl StoreFacts, id: &str, handle: Option<FileHandle>, to: &FolderPlace) -> Result<Option<Detection>, TreeError> {
    let Some(base) = facts.base_row(id)? else { return Ok(None) };
    let mut moved = move_in_onedrive(to, &base, handle);
    moved.same_content = !facts.has_update(id)?;
    Ok(Some(moved))
}

/// A local-only file is uploaded from `to`. One that is going up right now goes up as it is:
/// only where it is changed.
fn upload_from(facts: &impl StoreFacts, file: &DiskEntry, open: bool, to: &FolderPlace, now: i64) -> Result<Detection, TreeError> {
    let mut d = create_or_mkdir(to, file, open, now);
    if facts.outbox_rows_of(&file.handle)?.iter().any(|row| row.state == OutboxState::Running) {
        d.kind = OutboxKind::Move;
    }
    Ok(d)
}

/// A local-only entry stays on this computer from now on, with what is below it: what the
/// outbox held of it is removed, and so is every row of a local-only entry at and below the
/// place it came from that is not being sent.
fn keep_off_onedrive(facts: &impl StoreFacts, handle: &FileHandle, from: &Path, at: &Path, batch: &mut OutboxBatch) -> Result<(), TreeError> {
    batch.detections.extend(forget_local_only(facts, handle, at)?);
    let below = facts.rows_at_or_under(from)?;
    let unsent = below.iter().filter(|row| row.item_id.is_none() && row.state != OutboxState::Running);
    batch.ops.extend(unsent.map(|row| OutboxOp::Remove(row.seq)));
    Ok(())
}

#[cfg(test)]
mod tests;
