//! A file or a directory a program renamed or moved inside the folder.
//!
//! What is in OneDrive is moved there by the outbox. A local-only entry is uploaded from its
//! new place, or no longer uploaded when the new place is ignored. A local-only
//! file renamed over a OneDrive file is that file's new content: programs save that way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::TreeError;

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{detect_forgotten, detect_local_only_entries, detect_local_only_entry, detect_modified, detect_moved, detect_removed_at, detect_removed_by_id, rebase, wait_for_writer};
use super::{FolderEvent, OutboxBatch, StoreFacts, WriteState};
use crate::fuse::backing_directory::{DiskEntry, EntryKind};

/// A rename that was made in the folder.
#[derive(Debug)]
pub(in crate::fuse) enum EntryRenamed {
    /// A local-only file was renamed over the OneDrive file `over`, to the place `to`: the
    /// file is that file's new content, and what the outbox held of the local-only file
    /// itself is removed — it is never sent under its own name.
    SavedOverOneDriveFile { file: DiskEntry, write_state: WriteState, over: String, to: EntryPlace },
    /// Anything else: `entry` moved from `from` to `to`, where `displaced` was.
    Moved { entry: MovedEntry, from: PathBuf, to: EntryPlace, displaced: Option<DisplacedEntry> },
}

/// What moved.
#[derive(Debug)]
pub(in crate::fuse) enum MovedEntry {
    /// A file or a directory of OneDrive: moved in OneDrive. `handle` is of what the backing
    /// directory holds for it, now at the new place: its downloaded copy, or its directory.
    InOneDrive { id: String, kind: EntryKind, handle: Option<FileHandle> },
    /// A local-only file, to a place it is uploaded from.
    LocalOnlyFile { file: DiskEntry, write_state: WriteState },
    /// A local-only file, to a place where it is ignored from now on.
    IgnoredLocalOnlyFile { file: DiskEntry },
    /// A local-only directory, to a place it is uploaded from: `uploaded_entries` is what of
    /// it is uploaded from there, itself first, then all it holds.
    LocalOnlyDirectory { uploaded_entries: Vec<LocalOnlyEntry> },
    /// A local-only directory, to a place where it is ignored from now on, with all it
    /// holds.
    IgnoredLocalOnlyDirectory { handle: FileHandle },
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
            EntryRenamed::SavedOverOneDriveFile { file, write_state, over, to } => detect_saved_over(facts, &file, write_state, &over, &to, now),
            EntryRenamed::Moved { entry, from, to, displaced } => {
                let at = to.path();
                // What was at the new place first: it frees the name.
                let mut batch = OutboxBatch::of(match &displaced {
                    Some(DisplacedEntry::InOneDrive(id)) => detect_removed_by_id(facts, id, &at)?,
                    Some(DisplacedEntry::LocalOnly(handle)) => detect_forgotten(facts, handle, &at)?,
                    None => Vec::new(),
                });
                let kind = match entry {
                    MovedEntry::InOneDrive { id, kind, handle } => {
                        batch.detections.extend(detect_moved_by_id(facts, &id, handle, &to)?);
                        kind
                    }
                    MovedEntry::LocalOnlyFile { file, write_state } => {
                        batch.detections.push(detect_uploaded_from(facts, &file, write_state, &to, now)?);
                        EntryKind::File
                    }
                    MovedEntry::IgnoredLocalOnlyFile { file } => {
                        detect_kept_off_onedrive(facts, &file.handle, &from, &at, &mut batch)?;
                        EntryKind::File
                    }
                    MovedEntry::IgnoredLocalOnlyDirectory { handle } => {
                        detect_kept_off_onedrive(facts, &handle, &from, &at, &mut batch)?;
                        EntryKind::Directory
                    }
                    MovedEntry::LocalOnlyDirectory { uploaded_entries } => {
                        batch.detections.extend(detect_local_only_entries(&uploaded_entries, now));
                        EntryKind::Directory
                    }
                };
                // A directory takes the rows of what is below it along.
                if kind == EntryKind::Directory {
                    batch.ops.push(rebase(&from, &at));
                }
                Ok(batch)
            }
        }
    }
}

/// Save by rename. Nothing when the base no longer has the file.
fn detect_saved_over(facts: &impl StoreFacts, file: &DiskEntry, write_state: WriteState, over: &str, to: &EntryPlace, now: i64) -> Result<OutboxBatch, TreeError> {
    let Some(base) = facts.base_row(over)? else { return Ok(OutboxBatch::default()) };
    let pending = facts.outbox_rows_of(&file.handle)?;
    let ops = pending.iter().filter(|row| row.state != OutboxState::Running).map(|row| OutboxOp::Remove(row.seq)).collect();
    let mut update = detect_modified(to, &base, file);
    if write_state == WriteState::OpenForWriting {
        wait_for_writer(&mut update, now);
    }
    Ok(OutboxBatch { detections: vec![update], ops })
}

/// The file or directory of OneDrive `id` is at `to` now. Its content is the base's unless
/// the outbox holds new content of it. Nothing when the base no longer has it.
fn detect_moved_by_id(facts: &impl StoreFacts, id: &str, handle: Option<FileHandle>, to: &EntryPlace) -> Result<Option<Detection>, TreeError> {
    let Some(base) = facts.base_row(id)? else { return Ok(None) };
    let mut moved = detect_moved(to, &base, handle);
    moved.same_content = !facts.has_update(id)?;
    Ok(Some(moved))
}

/// A local-only file is uploaded from `to`. One that is going up right now goes up as it is:
/// only where it is changed.
fn detect_uploaded_from(facts: &impl StoreFacts, file: &DiskEntry, write_state: WriteState, to: &EntryPlace, now: i64) -> Result<Detection, TreeError> {
    let mut d = detect_local_only_entry(to, file, write_state, now);
    if facts.outbox_rows_of(&file.handle)?.iter().any(|row| row.state == OutboxState::Running) {
        d.kind = OutboxKind::Move;
    }
    Ok(d)
}

/// A local-only entry is ignored from now on, with what is below it: every row of it, and of
/// a local-only entry at and below the place it came from, that is not being sent is removed
/// from the outbox, and no row is left of it. A row of the entry itself that is being sent
/// is left to finish, and is followed by the removal of what it made in OneDrive.
fn detect_kept_off_onedrive(facts: &impl StoreFacts, handle: &FileHandle, from: &Path, at: &Path, batch: &mut OutboxBatch) -> Result<(), TreeError> {
    let own = facts.outbox_rows_of(handle)?;
    let below = facts.rows_at_or_under(from)?;
    if own.iter().any(|row| row.state == OutboxState::Running) {
        batch.detections.push(detect_removed_at(None, Some(handle.clone()), at));
    }
    let unsent = own.iter().chain(below.iter().filter(|row| row.item_id.is_none())).filter(|row| row.state != OutboxState::Running);
    let seqs: BTreeSet<i64> = unsent.map(|row| row.seq).collect();
    batch.ops.extend(seqs.into_iter().map(OutboxOp::Remove));
    Ok(())
}

#[cfg(test)]
mod tests;
