//! What a change made in the folder is recorded as: its detections, and what goes into the
//! outbox with them.
//!
//! Everything here is a decision and nothing else: a function of a typed event — an enum that
//! names what happened — and of a few facts of the store. No disk, no mount, no inode. Whoever
//! made the change builds the event, owned and complete, from what it saw; the decision then
//! runs inside the store's transaction that records it ([`super::recorder`]), reading the
//! facts it needs through [`StoreFacts`], so that what it reads and what it writes are one
//! state.
//!
//! The events, each in a module of its own:
//!
//! - [`made`]: a file or directory a program made ([`Made`]);
//! - [`removed`]: one it removed ([`Removed`]);
//! - [`renamed`]: one it renamed or moved, and what that displaced ([`Renamed`]);
//! - [`written`]: a file it opened for writing, closed or cut ([`Written`]);
//! - [`unrecorded_objects`]: the local objects found in the folder that the outbox has no row for
//!   ([`UnrecordedObjects`]);
//! - [`kept_copy`]: a local object that a change from OneDrive met, and is kept
//!   ([`KeptCopy`]).
//!
//! Here is what they share: a [`FolderPlace`]; a file or directory OneDrive has no item for yet, as
//! far as a row goes ([`LocalObject`]), and one at its place ([`LocalEntry`]); the detections
//! of the plain facts — a local object stands here, an item stands here, something was
//! deleted — and when a row of content is due ([`wait_for_writer`], [`wait_for_quiet`]).
//!
//! What is uploaded at all is not decided here ([`super::upload_rules`]): an event says it
//! as a fact. The names OneDrive refuses are: a refused name is a reason on its row.

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::{Row, Table, TreeError, TreeStore};

use crate::local::names;

mod kept_copy;
mod made;
mod removed;
mod renamed;
mod unrecorded_objects;
mod written;

pub(super) use kept_copy::KeptCopy;
pub(super) use made::Made;
pub(super) use removed::Removed;
pub(super) use renamed::{DisplacedEntry, MovedEntry, Renamed};
pub(super) use unrecorded_objects::UnrecordedObjects;
pub(super) use written::{ItemWrite, LocalWrite, Written};

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
    /// The item `id` as the base has it: what a change to it is made against.
    fn base_item(&self, id: &str) -> Result<Option<Row>, TreeError>;
    /// The outbox's rows of the local object `object`.
    fn outbox_rows_of(&self, object: &FileHandle) -> Result<Vec<OutboxRow>, TreeError>;
    /// The outbox's rows at the place `rel` and below it.
    fn rows_at_or_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError>;
    /// Whether the outbox holds new content of item `id`.
    fn has_update(&self, id: &str) -> Result<bool, TreeError>;
}

impl StoreFacts for TreeStore {
    fn base_item(&self, id: &str) -> Result<Option<Row>, TreeError> {
        self.get(Table::Items, id)
    }

    fn outbox_rows_of(&self, object: &FileHandle) -> Result<Vec<OutboxRow>, TreeError> {
        self.outbox_for_object(object)
    }

    fn rows_at_or_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        self.outbox_at_or_under(rel)
    }

    fn has_update(&self, id: &str) -> Result<bool, TreeError> {
        Ok(self.outbox_for_item(id)?.iter().any(|row| row.kind == OutboxKind::Update))
    }
}

/// A place in the folder: a name in a directory, and that directory's item when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FolderPlace {
    pub(super) above: Option<String>,
    pub(super) dir: PathBuf,
    pub(super) name: String,
}

impl FolderPlace {
    pub(super) fn new(above: Option<&str>, dir: &Path, name: &str) -> Self {
        FolderPlace { above: above.map(str::to_owned), dir: dir.to_path_buf(), name: name.to_owned() }
    }

    pub(super) fn rel(&self) -> PathBuf {
        self.dir.join(&self.name)
    }
}

/// A file or directory of the backing directory that OneDrive has no item for yet, as far as
/// a row goes: the file handle that is what it is, and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalObject {
    pub(super) handle: FileHandle,
    pub(super) kind: ObjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObjectKind {
    File { size: u64 },
    Dir,
}

impl LocalObject {
    fn size(&self) -> Option<u64> {
        match self.kind {
            ObjectKind::File { size } => Some(size),
            ObjectKind::Dir => None,
        }
    }

    fn is_dir(&self) -> bool {
        self.kind == ObjectKind::Dir
    }
}

/// A local object at the place it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalEntry {
    pub(super) place: FolderPlace,
    pub(super) object: LocalObject,
}

/// The item as the base has it: what a change to it was made against.
fn base(row: &Row) -> Base {
    Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: row.parent_id.clone(), name: Some(row.name.clone()) }
}

fn detection_at(place: &FolderPlace, kind: OutboxKind, object: Option<FileHandle>, size: Option<u64>) -> Detection {
    Detection {
        kind,
        item_id: None,
        object,
        rel: place.rel(),
        base: None,
        target_parent: place.above.clone(),
        target_name: Some(place.name.clone()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size,
    }
}

/// The local object `object` stands at `place`: a file to create, a directory to make. A
/// name OneDrive refuses, or a file too large for it, is said on the row and waits for the
/// user; a file a program still writes (`open`) waits for its last writer, and one just
/// closed is due a moment later.
fn create_or_mkdir(place: &FolderPlace, object: &LocalObject, open: bool, now: i64) -> Detection {
    let kind = if object.is_dir() { OutboxKind::Mkdir } else { OutboxKind::Create };
    let mut d = detection_at(place, kind, Some(object.handle.clone()), object.size());
    let too_large = object.size().is_some_and(|size| size > names::MAX_FILE_SIZE);
    let refused = names::refused(place.name.as_ref()).or(too_large.then_some(names::Refused::TooLarge));
    if let Some(refused) = refused {
        d.state = OutboxState::Blocked;
        d.reason = Some(refused.reason());
    } else if open {
        wait_for_writer(&mut d, now);
    } else if !object.is_dir() {
        wait_for_quiet(&mut d, now);
    }
    d
}

/// The detections of `entries`, each by the rules of a new local object that no program
/// holds open: a directory that is uploaded from now on, and all it holds.
pub(super) fn create_entries(entries: &[LocalEntry], now: i64) -> Vec<Detection> {
    entries.iter().map(|entry| create_or_mkdir(&entry.place, &entry.object, false, now)).collect()
}

/// The item `row` stands at `place`, in `object` when the backing directory holds one for
/// it: a move, and how an item is said to be where it is.
fn move_item(place: &FolderPlace, row: &Row, object: Option<FileHandle>) -> Detection {
    Detection { item_id: Some(row.id.clone()), base: Some(base(row)), ..detection_at(place, OutboxKind::Move, object, None) }
}

/// The file `file` at `place` is the new content of the item `row`.
fn update_item(place: &FolderPlace, row: &Row, file: &LocalObject) -> Detection {
    let update = detection_at(place, OutboxKind::Update, Some(file.handle.clone()), file.size());
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

/// What was at `rel` is removed: the item `row`, or a local object.
fn delete(row: Option<&Row>, object: Option<FileHandle>, rel: &Path) -> Detection {
    Detection {
        kind: OutboxKind::Delete,
        item_id: row.map(|r| r.id.clone()),
        object,
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

/// The item `id` was removed from `rel` here. Nothing for an item the base no longer has.
fn delete_item(facts: &impl StoreFacts, id: &str, rel: &Path) -> Result<Vec<Detection>, TreeError> {
    Ok(facts.base_item(id)?.iter().map(|base| delete(Some(base), None, rel)).collect())
}

/// A local object is gone from `rel`, or stays on this computer from now on: what the
/// outbox held of it goes. Nothing for an object the outbox never heard of.
fn forget_object(facts: &impl StoreFacts, object: &FileHandle, rel: &Path) -> Result<Vec<Detection>, TreeError> {
    Ok(if facts.outbox_rows_of(object)?.is_empty() { Vec::new() } else { vec![delete(None, Some(object.clone()), rel)] })
}

/// Whether a directory that carries `carried` is the directory of the folder item `id`,
/// which went from OneDrive: one that carries nothing is, one that carries another item's
/// id is that item's and is left alone.
pub(super) fn is_folder_of(carried: Option<&str>, id: &str) -> bool {
    carried.is_none_or(|carried| carried == id)
}

/// A directory moved: the rows of what is below it follow.
pub(super) fn rebase(from: &Path, to: &Path) -> OutboxOp {
    OutboxOp::Rebase { from: from.to_path_buf(), to: to.to_path_buf() }
}

#[cfg(test)]
pub(super) mod tests;
