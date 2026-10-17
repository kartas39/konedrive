//! The local content of an item as a step towards OneDrive reads it, and the marks a
//! confirmed step leaves on what it sent.
//!
//! **State owned:** none; it reaches the backing directory by the path of the item's
//! local version (`docs/design/mount.md` §3.2: a row finds its file by the path of its
//! local version; no file handle is stored), through `PlaceChanges::open_at_row_path`: the
//! file it opens is the file of the row it read, whatever is renamed meanwhile. It is the
//! one way a part outside the mount gets at a backing file.
//!
//! **The rows are read on a connection of the call's own**, opened for reading alone
//! before `open_at_row_path` is asked and closed with the call: the read closure that
//! function runs — on its last try under the lock every write operation and every step
//! begins its place change under — waits for nothing but its own read. Never the store's
//! one reading thread, which the plan and the bus read through for as long as they need.
//! The connection sees what was committed before it was opened: the versions a step was
//! planned from, the commit a mark follows.
//!
//! **Answers for:** the bytes a step sends and hashes being those of the version it was
//! planned from; and the marks after an upload (`docs/design/mount.md` §3.5): the
//! OneDrive item id, the whole-copy state and the cTag the content was confirmed as
//! (whatever agrees a content writes all three).
//!
//! **Tested without the rest**, over the fixture (`item_fixture`):
// test: the_content_is_opened_at_the_path_of_the_local_version
// test: a_file_renamed_twice_between_the_row_and_the_open_is_still_the_rows_file
// test: a_file_whose_size_or_time_is_not_the_planned_contents_is_not_opened
// test: a_file_a_program_holds_open_for_writing_is_not_opened
// test: a_fragment_read_after_the_file_changed_says_so
// test: the_hash_of_the_content_is_the_quickxorhash_of_its_bytes
// test: a_sent_file_is_marked_with_the_item_id_the_whole_copy_state_and_the_ctag
// test: a_file_freed_or_renamed_during_its_upload_is_marked_through_its_descriptor
// test: a_directory_made_in_onedrive_is_marked_with_its_item_id_when_it_is_on_disk
// test: an_item_that_gave_its_binding_up_loses_its_marks
// test: the_rows_are_read_without_waiting_for_the_stores_reading_thread
// test: the_time_a_file_is_given_is_its_contents_with_the_nanoseconds

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};
use konedrive_graph::quickxor::QuickXor;
use konedrive_tree::{ConfirmedItem, ContentPresence, ContentVersion, Item, ItemKind, ItemVersions, LocalItemId, TreeError, TreeStore};

use super::item_handles::ItemHandles;
use super::place_changes::{decide_open_trouble, OpenAtRowError, PlaceChanges};
use crate::sync_parts::step_outcome::{StepFailure, StepObstacle, StepTrouble};

/// How many bytes of a file are hashed at a time.
const HASHED_AT_ONCE: usize = 1 << 20;

/// Whether a file of `size` bytes, last modified at `mtime` and `mtime_nsec`, has the size
/// and the time of `content`. The nanoseconds are asked only of a content that has any: a
/// time OneDrive gave, and one set in whole seconds, has none, and the file may then carry
/// what the filesystem kept. Pure.
pub(super) fn has_size_and_time_of(content: &ContentVersion, size: u64, mtime: i64, mtime_nsec: u32) -> bool {
    let (content_mtime, content_mtime_nsec) = content.mtime();
    content.size() == size && content_mtime == mtime && (content_mtime_nsec == 0 || content_mtime_nsec == mtime_nsec)
}

/// The time a file that holds `content` is given: the content's, with its nanoseconds, so
/// that the file has the size and the time of its row's content to the nanosecond
/// ([`has_size_and_time_of`]). Pure.
pub(super) fn to_content_time(content: &ContentVersion) -> std::time::SystemTime {
    let (mtime, mtime_nsec) = content.mtime();
    crate::clock::to_system_time(mtime) + std::time::Duration::from_nanos(u64::from(mtime_nsec))
}

/// The backing file of an item's local content, open for reading, as it was when it was
/// opened.
#[derive(Debug)]
pub(crate) struct LocalContentFile {
    item: LocalItemId,
    file: File,
    /// The content the step was planned from: every read asks that the file still has its
    /// size and its time.
    content: ContentVersion,
    /// Where the file was when it was opened, relative to the folder: for what a failure
    /// says.
    path: PathBuf,
}

impl LocalContentFile {
    /// The size of the content, in bytes.
    pub(crate) fn size(&self) -> u64 {
        self.content.size()
    }

    fn fail(&self, error: &io::Error) -> StepTrouble {
        StepTrouble::Failure(StepFailure::Disk { errno: error.raw_os_error().unwrap_or(libc::EIO), path: self.path.clone() })
    }

    /// Asks that the file still has the size and the time of the planned content.
    fn check_content(&self) -> Result<(), StepTrouble> {
        let metadata = self.file.metadata().map_err(|error| self.fail(&error))?;
        match has_size_and_time_of(&self.content, metadata.len(), metadata.mtime(), metadata.mtime_nsec() as u32) {
            true => Ok(()),
            false => Err(StepTrouble::Obstacle(StepObstacle::ContentChanged)),
        }
    }

    /// `len` bytes from `offset`, after asking that the file's size and time are still the
    /// planned content's, and asking it again after them:
    /// bytes read while the file changed are not answered. A free-up of the same file
    /// cannot come in between: free-up leaves a file a step runs for. Blocking: called
    /// off the runtime.
    pub(crate) fn read_fragment(&self, offset: u64, len: usize) -> Result<Vec<u8>, StepTrouble> {
        self.check_content()?;
        let mut bytes = vec![0; len];
        match self.file.read_exact_at(&mut bytes, offset) {
            Ok(()) => {}
            // Shorter than its content by now.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Err(StepTrouble::Obstacle(StepObstacle::ContentChanged)),
            Err(error) => return Err(self.fail(&error)),
        }
        self.check_content()?;
        Ok(bytes)
    }

    /// The QuickXorHash of the whole content, base64. Blocking; no lock is held. Called by
    /// `RowStep::HashLocalContent` and
    /// by a step that asks whether what OneDrive has is what it sent.
    pub(crate) fn hash(&self) -> Result<String, StepTrouble> {
        self.check_content()?;
        let (mut hash, mut hashed, size) = (QuickXor::new(), 0, self.size());
        let mut bytes = vec![0; HASHED_AT_ONCE];
        while hashed < size {
            let wanted = HASHED_AT_ONCE.min((size - hashed) as usize);
            match self.file.read_exact_at(&mut bytes[..wanted], hashed) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Err(StepTrouble::Obstacle(StepObstacle::ContentChanged)),
                Err(error) => return Err(self.fail(&error)),
            }
            hash.update(&bytes[..wanted]);
            hashed += wanted as u64;
        }
        self.check_content()?;
        Ok(hash.finish_base64())
    }
}

/// What a mark is written on: the entry of a row, open, with the row as it was read with
/// its path.
struct MarkedEntry {
    entry: File,
    row: Item,
}

/// The mark `mark` is taken off `entry`; one that is not there is no error.
fn remove_mark(entry: &File, mark: &str) -> io::Result<()> {
    match xattr::FileExt::remove_xattr(entry, mark) {
        Err(error) if error.raw_os_error() != Some(libc::ENODATA) => Err(error),
        _ => Ok(()),
    }
}

/// Marks `file` as the whole copy of `content` of the item of OneDrive `onedrive_item_id`:
/// the cTag, when the content has one, the whole-copy state, and the item id last — an id
/// with no state is what an entry that holds nothing of the content carries. Not synced.
pub(super) fn write_whole_copy_marks(file: &File, onedrive_item_id: &str, content: &ContentVersion) -> io::Result<()> {
    match content {
        ContentVersion::Hashed { ctag: Some(ctag), .. } => placeholder::write_ctag(file, ctag)?,
        // No cTag is known of this content: one of another content says nothing true.
        ContentVersion::Hashed { ctag: None, .. } | ContentVersion::ChangedHere { .. } => placeholder::with_owner_write(file, || remove_mark(file, XATTR_CTAG))?,
    }
    placeholder::write_state(file, State::Hydrated)?;
    placeholder::write_item_id(file, onedrive_item_id)
}

/// The local contents of one folder.
pub(crate) struct ItemContentFiles {
    place_changes: Arc<PlaceChanges>,
    /// Where the store is: each call reads the rows on a connection of its own.
    store_path: PathBuf,
    item_handles: Arc<ItemHandles>,
}

impl ItemContentFiles {
    /// Made by the mount's build, with the store's path.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store_path: PathBuf, item_handles: Arc<ItemHandles>) -> Self {
        Self { place_changes, store_path, item_handles }
    }

    /// A connection to the store for reading alone, for one call (the module's doc).
    fn open_read_connection(&self) -> Result<TreeStore, TreeError> {
        TreeStore::open_read_only(&self.store_path)
    }

    /// Opens the local content of the file `item` was planned as: through
    /// `PlaceChanges::open_at_row_path`, whose read closure — over a connection of this
    /// call's own — answers the path only while the row's local content is still the
    /// planned one; then no program may hold the file open for writing, and the file must
    /// have that content's size and time. An error of the open is turned by
    /// `place_changes::decide_open_trouble`. Blocking; it waits for no other reader of
    /// the store.
    ///
    /// A row whose local content is another by now is answered as
    /// `StepObstacle::OpenForWriting` while a program writes the file, and as
    /// `StepObstacle::ContentChanged` otherwise: `StepTrouble` has no case for rows that
    /// changed, and the step's commit would find them changed anyway.
    ///
    /// Called by a step towards OneDrive that sends content and by
    /// `RowStep::HashLocalContent`, before any request and with no lock; never on the
    /// write-operation thread and never on a thread of a runtime.
    pub(crate) fn open_to_send(&self, item: &ItemVersions) -> Result<LocalContentFile, StepTrouble> {
        let id = item.local_item_id;
        let Some(content) = item.local.as_ref().and_then(|local| local.content.clone()) else { return Err(StepTrouble::Obstacle(StepObstacle::ContentChanged)) };
        let rows = self.open_read_connection().map_err(|error| decide_open_trouble(&OpenAtRowError::Store(error)))?;
        let find_path = || {
            rows.read_items(|reader| {
                let is_planned_content = reader.find_item(id)?.is_some_and(|row| row.kind == ItemKind::File && row.local.is_some_and(|local| local.content.as_ref() == Some(&content)));
                match is_planned_content {
                    true => Ok(reader.find_path(id)?.map(|path| (path.clone(), path))),
                    false => Ok(None),
                }
            })
        };
        let opened = self.place_changes.open_at_row_path(find_path).map_err(|error| decide_open_trouble(&error))?;
        let is_open_for_writing = self.item_handles.is_open_for_writing(id);
        let Some((file, path)) = opened.filter(|_| !is_open_for_writing) else {
            let obstacle = if is_open_for_writing { StepObstacle::OpenForWriting } else { StepObstacle::ContentChanged };
            return Err(StepTrouble::Obstacle(obstacle));
        };
        let local_content = LocalContentFile { item: id, file, content, path };
        let is_file = local_content.file.metadata().map_err(|error| local_content.fail(&error))?.is_file();
        if !is_file {
            return Err(StepTrouble::Obstacle(StepObstacle::BackingFileMissing));
        }
        local_content.check_content()?;
        Ok(local_content)
    }

    /// Marks the file that was sent with what OneDrive confirmed: the item id, the
    /// whole-copy state and the cTag, synced, through the descriptor it was read through, wherever the
    /// file is by now. Blocking, with NO lock: it goes through the descriptor, and one
    /// step per item keeps `remove_onedrive_marks` away. Called by a step towards
    /// OneDrive after its commit was written; a failure is logged: the marks serve the
    /// check at start and a lost store, not the running daemon (`docs/design/mount.md` §3.5).
    ///
    /// The file is given the time of the confirmed content too: the commit wrote that
    /// content as the row's local one, and the check at start reads a copy as clean by its
    /// size and its time in whole seconds (OneDrive may have kept another time than was
    /// sent). A file that has not the sent content's size and time any more — a program
    /// changed it since — or that a program holds open for writing is left as it is: its
    /// row says its own content.
    pub(crate) fn mark_sent_file(&self, file: &LocalContentFile, sent: &ConfirmedItem) -> std::io::Result<()> {
        let Some(confirmed) = &sent.content else { return Err(io::Error::new(io::ErrorKind::InvalidInput, "a file was confirmed with no content")) };
        let metadata = file.file.metadata()?;
        let is_sent_content = has_size_and_time_of(&file.content, metadata.len(), metadata.mtime(), metadata.mtime_nsec() as u32) && confirmed.size() == metadata.len();
        if !is_sent_content || self.item_handles.is_open_for_writing(file.item) {
            return Ok(());
        }
        placeholder::set_mtime(&file.file, to_content_time(confirmed))?;
        write_whole_copy_marks(&file.file, sent.item.item_id.as_str(), confirmed)?;
        file.file.sync_all()
    }

    /// Opens what is on disk of the row `find_row` answers for `item`, at the path of its
    /// local version (`PlaceChanges::open_at_row_path`, to read: a mark is written through
    /// any descriptor, and a directory is opened no other way), the rows read on a
    /// connection of this call's own. `None` when `find_row`
    /// answers none, when the folder does not show the item and when nothing of it is on
    /// disk.
    fn open_to_mark(&self, item: LocalItemId, is_marked: fn(&Item) -> bool) -> io::Result<Option<MarkedEntry>> {
        let rows = self.open_read_connection().map_err(|error| io::Error::other(error.to_string()))?;
        let find_path = || {
            rows.read_items(|reader| {
                let Some(row) = reader.find_item(item)?.filter(is_marked) else { return Ok(None) };
                Ok(reader.find_path(item)?.map(|path| (path, row)))
            })
        };
        match self.place_changes.open_at_row_path(find_path) {
            Ok(opened) => Ok(opened.map(|(entry, row)| MarkedEntry { entry, row })),
            Err(OpenAtRowError::NoFile { .. }) => Ok(None),
            Err(OpenAtRowError::Store(error)) => Err(io::Error::other(error.to_string())),
            Err(OpenAtRowError::Disk { error, .. }) => Err(error),
        }
    }

    /// Marks the directory of `item` with the item id OneDrive gave it, when the directory
    /// is in the backing directory: opened through
    /// `PlaceChanges::open_at_row_path`. Called by the runner of `MakeDirectory` after its
    /// commit. Blocking, with no lock, as [`mark_sent_file`](Self::mark_sent_file).
    pub(crate) fn mark_made_directory(&self, item: &ItemVersions, made: &ConfirmedItem) -> std::io::Result<()> {
        let Some(marked) = self.open_to_mark(item.local_item_id, |row| row.kind == ItemKind::Directory)? else { return Ok(()) };
        // A row that has another binding by now is not the directory OneDrive made.
        if marked.row.onedrive_item.as_ref() != Some(&made.item) || !marked.entry.metadata()?.is_dir() {
            return Ok(());
        }
        placeholder::write_item_id(&marked.entry, made.item.item_id.as_str())?;
        marked.entry.sync_all()
    }

    /// Marks what is on disk of `item` as the whole copy of the content the two sides
    /// agree on now, with the binding's item id and the cTag: after `MergeItems` and
    /// `AgreeOnContent`. Opened through `PlaceChanges::open_at_row_path`.
    ///
    /// A directory gets the item id. A file gets the three marks, and the time of the
    /// content agreed on (as [`mark_sent_file`](Self::mark_sent_file) gives it), when its
    /// row says its content is on disk and is the one agreed on, the file has that size
    /// and no program writes it; any other file gets the item id alone: it is the entry of
    /// that item of OneDrive and no whole copy of what OneDrive has. Blocking, with no
    /// lock.
    pub(crate) fn mark_agreed_content(&self, item: LocalItemId) -> std::io::Result<()> {
        let Some(MarkedEntry { entry, row }) = self.open_to_mark(item, |row| row.onedrive_item.is_some())? else { return Ok(()) };
        let Some(onedrive_item) = &row.onedrive_item else { return Ok(()) };
        let metadata = entry.metadata()?;
        let agreed = row.local.as_ref().and_then(|local| local.content.as_ref()).filter(|content| row.synced_content.as_ref() == Some(*content));
        let whole_copy = agreed.filter(|content| {
            row.kind == ItemKind::File && metadata.is_file() && row.content_presence == ContentPresence::OnDisk && row.content_before_write.is_none() && content.size() == metadata.len() && !self.item_handles.is_open_for_writing(item)
        });
        match whole_copy {
            Some(content) => {
                placeholder::set_mtime(&entry, to_content_time(content))?;
                write_whole_copy_marks(&entry, onedrive_item.item_id.as_str(), content)?;
            }
            None if metadata.is_dir() == (row.kind == ItemKind::Directory) => placeholder::write_item_id(&entry, onedrive_item.item_id.as_str())?,
            None => return Ok(()),
        }
        entry.sync_all()
    }

    /// Takes the OneDrive marks off what is on disk of `item`: it gave its binding up and
    /// is an item OneDrive does not have. Called by
    /// the runner of `GiveUpBinding` after its commit. The item id first, then the
    /// whole-copy state and the cTag; what else konedrive keeps on the entry stays.
    /// Nothing for an item that has a binding by now. Blocking, with no lock.
    pub(crate) fn remove_onedrive_marks(&self, item: LocalItemId) -> std::io::Result<()> {
        let Some(MarkedEntry { entry, .. }) = self.open_to_mark(item, |row| row.onedrive_item.is_none())? else { return Ok(()) };
        placeholder::with_owner_write(&entry, || [XATTR_ITEM_ID, XATTR_STATE, XATTR_CTAG].into_iter().try_for_each(|mark| remove_mark(&entry, mark)))?;
        entry.sync_all()
    }
}

#[cfg(test)]
mod tests;
