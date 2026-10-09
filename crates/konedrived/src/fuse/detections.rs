//! What a change made in the folder is recorded as: its detections, and what goes into the
//! outbox with them.
//!
//! Everything here is a decision and nothing else: a function of a typed event — an enum that
//! names what happened — and of a few facts of the store. No disk, no mount, no node id.
//! Whoever made the change builds the event, owned and complete, from what it saw; the
//! decision then runs inside the store's transaction that records it
//! ([`super::outbox_recorder`]), reading the facts it needs through [`StoreFacts`], so that
//! what it reads and what it writes are one state.
//!
//! The events, each in a module of its own:
//!
//! - [`entry_created`]: a file or directory a program made ([`EntryCreated`]);
//! - [`entry_removed`]: one it removed ([`EntryRemoved`]);
//! - [`entry_renamed`]: one it renamed or moved, and what that displaced ([`EntryRenamed`]);
//! - [`file_written`]: a file it opened for writing, closed or cut ([`FileWritten`]);
//! - [`unrecorded_entries`]: the local-only entries found in the folder that the outbox has
//!   no row for ([`UnrecordedEntries`]);
//! - [`entry_kept_aside`]: a local-only entry that was in the way of a change from OneDrive,
//!   and is kept ([`EntryKeptAside`]).
//!
//! Here is what they share: the detections of the
//! plain facts — a local-only entry is here, a file or directory of OneDrive is here,
//! something was deleted — and when a row of content is due ([`wait_for_writer`],
//! [`wait_for_quiet`]).
//!
//! What is uploaded at all is not decided here ([`super::upload_rules`]): an event says it
//! as a fact. The names OneDrive refuses are: a refused name is a reason on its row.
//!
//! The store calls its rows items: `item_id` in a [`Detection`] is the OneDrive id, and
//! `object` the file handle of the entry on disk.

use std::path::Path;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::{Row, Table, TreeError, TreeStore};

use super::backing_directory::{DiskEntry, EntryKind};
use super::entry_place::EntryPlace;
use super::local_only_entry::LocalOnlyEntry;
use crate::local::names;

mod entry_created;
mod entry_kept_aside;
mod entry_removed;
mod entry_renamed;
mod file_written;
mod unrecorded_entries;

pub(super) use entry_created::EntryCreated;
pub(super) use entry_kept_aside::EntryKeptAside;
pub(super) use entry_removed::EntryRemoved;
pub(super) use entry_renamed::{DisplacedEntry, EntryRenamed, MovedEntry};
pub(super) use file_written::{FileWritten, LocalOnlyFileWrite, OneDriveFileWrite};
pub(super) use unrecorded_entries::UnrecordedEntries;

/// Whether a program holds a file open for writing at the moment a change of it is recorded:
/// a file that is, waits in the outbox for its last writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteState {
    /// A program holds it open for writing.
    OpenForWriting,
    /// No program does.
    NotOpen,
}

/// What one change puts into the outbox: its detections, in the order they are recorded, and
/// the operations on rows that go with them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OutboxBatch {
    pub(super) detections: Vec<Detection>,
    pub(super) ops: Vec<OutboxOp>,
}

impl OutboxBatch {
    fn of(detections: impl IntoIterator<Item = Detection>) -> Self {
        OutboxBatch { detections: detections.into_iter().collect(), ops: Vec::new() }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.detections.is_empty() && self.ops.is_empty()
    }
}

/// Something that happened in the folder, as whoever made it happen saw it. What it is
/// recorded as is decided from it and from the store, at the time `now`.
pub(super) trait FolderEvent: Send + 'static {
    fn detect(self, facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError>;
}

/// What a decision reads from the tree store.
pub(super) trait StoreFacts {
    /// The row of the OneDrive id `id` as the base has it: what a change to it is made
    /// against.
    fn base_row(&self, id: &str) -> Result<Option<Row>, TreeError>;
    /// The outbox's rows of the entry on disk that `handle` identifies.
    fn outbox_rows_of(&self, handle: &FileHandle) -> Result<Vec<OutboxRow>, TreeError>;
    /// The outbox's rows at the path `rel` and below it.
    fn rows_at_or_below(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError>;
    /// Whether the outbox holds new content of the OneDrive file `id`.
    fn has_update(&self, id: &str) -> Result<bool, TreeError>;
}

impl StoreFacts for TreeStore {
    fn base_row(&self, id: &str) -> Result<Option<Row>, TreeError> {
        self.get(Table::Items, id)
    }

    fn outbox_rows_of(&self, handle: &FileHandle) -> Result<Vec<OutboxRow>, TreeError> {
        self.outbox_for_object(handle)
    }

    fn rows_at_or_below(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        self.outbox_at_or_under(rel)
    }

    fn has_update(&self, id: &str) -> Result<bool, TreeError> {
        Ok(self.outbox_for_item(id)?.iter().any(|row| row.kind == OutboxKind::Update))
    }
}

/// The row as the base has it: what a change to it was made against.
fn base(row: &Row) -> Base {
    Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
}

fn detect_at(place: &EntryPlace, kind: OutboxKind, handle: Option<FileHandle>, size: Option<u64>) -> Detection {
    Detection {
        kind,
        item_id: None,
        object: handle,
        rel: place.path(),
        base: None,
        target_parent: place.directory.onedrive_id.clone(),
        target_name: Some(place.name.clone()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size,
    }
}

/// The entry `on_disk`, which is not in OneDrive yet, is at `place`: a file to create, a
/// directory to make. A name OneDrive refuses, or a file too large for it, is said on the row
/// and waits for the user; a file a program still writes waits for its last writer, and one
/// that is not open is due a moment later.
fn detect_local_only_entry(place: &EntryPlace, on_disk: &DiskEntry, write_state: WriteState, now: i64) -> Detection {
    let (kind, size) = match on_disk.metadata.kind {
        EntryKind::Directory => (OutboxKind::Mkdir, None),
        EntryKind::File => (OutboxKind::Create, Some(on_disk.metadata.size)),
    };
    let mut d = detect_at(place, kind, Some(on_disk.handle.clone()), size);
    let too_large = size.is_some_and(|size| size > names::MAX_FILE_SIZE);
    let refused = names::refused(place.name.as_ref()).or(too_large.then_some(names::Refused::TooLarge));
    if let Some(refused) = refused {
        d.state = OutboxState::Blocked;
        d.reason = Some(refused.reason());
    } else if write_state == WriteState::OpenForWriting {
        wait_for_writer(&mut d, now);
    } else if on_disk.metadata.kind == EntryKind::File {
        wait_for_quiet(&mut d, now);
    }
    d
}

/// The detections of `entries`, each by the rules of a new local-only entry that no program
/// holds open: a directory that is uploaded from now on, and all it holds.
pub(super) fn detect_local_only_entries(entries: &[LocalOnlyEntry], now: i64) -> Vec<Detection> {
    entries.iter().map(|entry| detect_local_only_entry(&entry.place, &entry.on_disk, WriteState::NotOpen, now)).collect()
}

/// The file or directory of OneDrive that `row` is, is at `place`, with the entry `handle`
/// identifies when the backing directory holds one for it: a move, and how something of
/// OneDrive is said to be where it is.
fn detect_moved(place: &EntryPlace, row: &Row, handle: Option<FileHandle>) -> Detection {
    Detection { item_id: Some(row.id.clone()), base: Some(base(row)), ..detect_at(place, OutboxKind::Move, handle, None) }
}

/// The file `copy` at `place` is the new content of the OneDrive file `row`.
fn detect_modified(place: &EntryPlace, row: &Row, copy: &DiskEntry) -> Detection {
    let update = detect_at(place, OutboxKind::Update, Some(copy.handle.clone()), copy.metadata.file_size());
    Detection { item_id: Some(row.id.clone()), base: Some(base(row)), ..update }
}

/// The row waits for the last program writing its file: recorded ready when that one closes
/// it, and looked at again by the worker itself in case that is never heard.
pub(super) fn wait_for_writer(d: &mut Detection, now: i64) {
    d.state = OutboxState::Waiting;
    d.reason = Some(Reason::OpenForWriting);
    d.next_try = Some(now + crate::local::RECHECK.as_secs() as i64);
}

/// The row of content just closed is due a moment later: a program that saves through a
/// file of another name renames it right after closing it, and that name is never sent.
pub(super) fn wait_for_quiet(d: &mut Detection, now: i64) {
    d.state = OutboxState::Waiting;
    d.reason = Some(Reason::Changed);
    d.next_try = Some(now + crate::local::QUIET.as_secs() as i64);
}

/// What was at `rel` is removed: the file or directory of OneDrive that `row` is, or a
/// local-only entry by its `handle`.
fn detect_removed_at(row: Option<&Row>, handle: Option<FileHandle>, rel: &Path) -> Detection {
    Detection {
        kind: OutboxKind::Delete,
        item_id: row.map(|r| r.id.clone()),
        object: handle,
        rel: rel.to_path_buf(),
        base: row.map(base),
        target_parent: None,
        target_name: None,
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// The file or directory of OneDrive `id` was removed from `rel` here. Nothing when the base
/// no longer has it.
fn detect_removed_by_id(facts: &impl StoreFacts, id: &str, rel: &Path) -> Result<Vec<Detection>, TreeError> {
    Ok(facts.base_row(id)?.iter().map(|base| detect_removed_at(Some(base), None, rel)).collect())
}

/// A local-only entry is gone from `rel`: what the outbox held of it is removed. Nothing for
/// one the outbox never heard of.
fn detect_forgotten(facts: &impl StoreFacts, handle: &FileHandle, rel: &Path) -> Result<Vec<Detection>, TreeError> {
    Ok(if facts.outbox_rows_of(handle)?.is_empty() { Vec::new() } else { vec![detect_removed_at(None, Some(handle.clone()), rel)] })
}

/// Whether a directory on disk that is marked with `marked_with` backs the OneDrive
/// directory `id`, which was removed from OneDrive: one that is marked with nothing does,
/// one that is marked with another id backs that other directory and is left alone.
pub(super) fn is_directory_of(marked_with: Option<&str>, id: &str) -> bool {
    marked_with.is_none_or(|marked_with| marked_with == id)
}

/// A directory moved: the rows of what is below it follow.
pub(super) fn rebase(from: &Path, to: &Path) -> OutboxOp {
    OutboxOp::Rebase { from: from.to_path_buf(), to: to.to_path_buf() }
}

#[cfg(test)]
pub(super) mod tests;
