//! A file's content changed by a program: open for writing, truncate, the time set, and
//! the last close. The row first (`docs/design/mount.md` §3.3).
//!
//! **Decides:** `decide_open_change` and `decide_close_change`. **Writes:**
//! `OpenedForWriting` at the first writer's open, before a byte goes to disk;
//! `ClosedChanged` with the size and the times read from the backing file through the
//! handle, or `ClosedUnchanged`, at the last close; `Truncated` for a truncation with no
//! writer; `ItemStore::set_local_modified_time` for a time alone. One rule for both
//! access modes: a close is written whatever the mode is by then.
//!
//! **Three of these make a backing file**, and with it an entry and maybe the directories
//! above it: the first writer's open and the open to overwrite of a file that is not on
//! disk, and its truncation to nothing. Row first, so between the row and the file a
//! reader would find a row that says on disk over no file. Each therefore begins a place
//! change BEFORE its row is written and makes the file through `PlaceChange::make_entry`
//! AFTER it: the count is odd from the row to the file, a reader waits, and
//! `mend_missing_copy` is never asked about a file that is being made. A row refused:
//! nothing was made. A file that cannot be made after its row: the mount's take-down.
//! Where the file is on disk already nothing of a name changes and no change is begun;
//! the backing file of a row is opened on this thread, under the tree lock, where no
//! place change can run (through the `BackingDirectory` the write operations hold for
//! it: `PlaceChanges::open_at_row_path` is for the other threads).
//!
//! **What the made file carries.** The empty backing file of an empty file of OneDrive
//! that a writer opened IS that content whole: it gets the marks of a downloaded copy
//! (the item id, whole, the cTag) and the content's time, so that a close with nothing
//! written leaves a clean copy the row and the check at start agree on. The file made
//! for an overwrite or a truncation holds the user's own content from its making: it
//! gets the item id alone.
//!
//! **On record before the row**, for each of the three: `PlaceChangeRecord::MakeEntry {
//! kind: File, place, owner: MadeEntryOwner::Item(item) }`, written
//! (`ItemStore::record_place_change`) before the place change is begun. Here the row is
//! written BEFORE the file, so the record does not go with the row: it is taken by itself
//! (`ItemStore::forget_place_change_record`) after the file was made, and also when the
//! row was refused. A death between the row and the file: the check at start puts the row
//! of a writer's open back as it was before the open, not on disk
//! (`PlaceChangeRowWrite::ContentBeforeWrite`); a truncation keeps nothing to put back, and
//! its row, which says on disk over no file, is the check's to decide like any other.
//!
//! **The row's time and the file's are kept the same**: whatever writes a time into the
//! row (a truncation, a time set) gives the backing file that very time after it changed
//! the file, so that a step towards OneDrive finds the content it was planned from by its
//! size and time.
//!
//! **Tested without the rest:**
// test: each_case_of_a_writers_open_is_decided
// test: each_case_of_the_last_close_is_decided
// test: each_case_of_what_is_downloaded_before_a_write_is_decided
// test: the_first_writers_open_raises_the_change_count_before_the_file_is_opened
// test: a_second_writers_open_writes_no_row
// test: a_file_not_on_disk_and_not_empty_is_downloaded_before_it_is_opened_for_writing
// test: an_empty_file_not_on_disk_is_opened_and_said_on_disk_in_one_transaction
// test: a_reader_between_the_row_that_says_on_disk_and_the_file_being_made_waits_and_gets_the_file
// test: a_file_opened_to_be_overwritten_is_not_downloaded
// test: the_last_close_of_a_changed_file_writes_the_size_and_times_of_the_backing_file
// test: the_last_close_of_an_untouched_file_puts_the_content_before_the_write_back
// test: a_close_while_another_writer_holds_the_file_writes_nothing
// test: the_writer_leaves_the_handles_only_after_the_row_of_its_last_close_is_written
// test: a_truncation_with_no_writer_writes_the_new_size_before_the_file_is_cut
// test: a_truncated_file_has_the_time_its_row_was_given
// test: a_truncation_of_a_file_open_for_writing_marks_it_written_and_writes_no_row
// test: a_truncation_through_a_handle_whose_row_is_gone_cuts_the_handles_file
// test: a_time_set_alone_goes_to_the_backing_file_and_to_the_row
// test: a_close_in_a_folder_switched_to_read_only_meanwhile_is_still_written
// test: an_empty_file_opened_and_closed_untouched_is_a_clean_copy_again
// test: a_backing_file_that_cannot_be_made_after_its_row_asks_for_the_take_down
// test: a_time_set_through_a_writer_is_written_by_its_close
// test: what_is_downloaded_before_a_write_is_read_without_waiting_for_the_stores_reading_thread
// test: bytes_written_through_a_handle_that_was_never_closed_are_unsent_after_the_check_at_start_though_size_and_time_stand
// test: a_file_opened_for_writing_and_never_closed_is_changed_here_after_the_check_at_start_also_when_nothing_was_written

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use fuser::Errno;
use konedrive_fs::placeholder::{self, State};
use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange, MadeEntryOwner, PlaceChangeRecord};

use super::super::backing_directory::EntryKind;
use super::super::errno::to_errno;
use super::super::item_downloader::ItemToDownload;
use super::super::item_handles::ClosingWriter;
use super::super::mount_take_down::TakeDownCause;
use super::super::item_handles::{CloseOutcome, Fh, WriterRank};
use super::super::place_changes::DirectoryMarks;
use super::{read_size_and_time, to_system_time, ItemWriteOperations};

#[cfg(test)]
mod tests;

/// A file whose whole content must be on disk before a program writes into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) enum FileToFill {
    /// Its content is on disk, or it is empty, or it is to be cut to nothing.
    Nothing,
    /// It is downloaded first.
    Download(ItemToDownload),
}

/// A writer that was opened.
pub(in crate::mount) struct OpenedWriter {
    pub(in crate::mount) fh: Fh,
    pub(in crate::mount) backing_file: Arc<File>,
}

/// What the row is told when a writer opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpenChange {
    /// Another handle writes the file already: `OpenedForWriting` was written.
    Nothing,
    /// The first writer of a file whose content is on disk: `OpenedForWriting`.
    OpenedForWriting,
    /// The first writer of a file whose content is not on disk and that is empty or is to
    /// be overwritten: `OpenedForWriting` and `OnDisk` in one transaction
    /// (`ItemStore::apply_with_content_on_disk`), then the backing file is made.
    OpenedForWritingAndOnDisk,
}

/// What a writer's open writes. Pure.
pub(super) fn decide_open_change(rank: WriterRank, content_presence: ContentPresence) -> OpenChange {
    match (rank, content_presence) {
        (WriterRank::Later, _) => OpenChange::Nothing,
        (WriterRank::First, ContentPresence::OnDisk) => OpenChange::OpenedForWriting,
        (WriterRank::First, ContentPresence::NotOnDisk) => OpenChange::OpenedForWritingAndOnDisk,
    }
}

/// What the last close writes, from what the close will leave (`ClosingWriter::outcome`)
/// and the backing file's size and times; `None` while another handle still writes, and
/// for an item the folder does not show any more. Pure.
pub(super) fn decide_close_change(closing: &ClosingWriter, is_shown: bool, size: u64, mtime: i64, mtime_nsec: u32) -> Option<LocalVersionChange> {
    match (closing.outcome, is_shown) {
        (CloseOutcome::StillOpenElsewhere, _) | (_, false) => None,
        (CloseOutcome::ContentChanged, true) => Some(LocalVersionChange::ClosedChanged { item: closing.item, size, mtime, mtime_nsec }),
        (CloseOutcome::ContentUntouched, true) => Some(LocalVersionChange::ClosedUnchanged { item: closing.item }),
    }
}

/// Whether the file of `row` must be downloaded whole before it is opened for writing or
/// truncated to `new_size` (`None`: opened): its content is not on disk, it is not empty,
/// and it is not cut to nothing. `ESTALE` for an item the folder does not show, `EISDIR`
/// for a directory. Pure.
pub(super) fn decide_file_to_fill(row: &Item, new_size: Option<u64>) -> Result<FileToFill, Errno> {
    let local = row.local.as_ref().ok_or(Errno::ESTALE)?;
    let content = local.content.as_ref().filter(|_| row.kind == ItemKind::File).ok_or(Errno::EISDIR)?;
    if row.content_presence == ContentPresence::OnDisk || content.size() == 0 || new_size == Some(0) {
        return Ok(FileToFill::Nothing);
    }
    // A file that is not on disk is one of OneDrive's: its content has nowhere else to be.
    let onedrive_item = row.onedrive_item.clone().ok_or(Errno::EIO)?;
    Ok(FileToFill::Download(ItemToDownload { item: row.local_item_id, onedrive_item, content: content.clone() }))
}

/// A file the folder shows, as an operation on its content read it in one job of the
/// store.
struct ShownFile {
    row: Item,
    place: ItemPlace,
    content: ContentVersion,
    /// Its path in the backing directory, and the marks of the directories on it.
    path: PathBuf,
    directory_marks: DirectoryMarks,
}

/// What a backing file made for a row is made as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MadeFileContent {
    /// The whole content of an empty file of OneDrive: the marks of a downloaded copy and
    /// the content's time.
    EmptyCopy,
    /// The user's own from its making (an overwrite, a truncation): the item id alone.
    ChangedHere,
}

impl ItemWriteOperations {
    /// Whether the file `node` must be downloaded whole before it is opened for writing
    /// or truncated to `new_size` (`None`: opened): [`decide_file_to_fill`] of its row. A
    /// read-only folder refuses here, before anything is downloaded for a write it will
    /// not take. Called on the mount's thread, before the job is queued; the download
    /// then runs on the runtime and the job after it. Read through the calling thread's own reading
    /// connection (`ReadConnections::read`), as a lookup reads: it waits for neither the
    /// store's thread nor the one reading thread the plan and the bus share.
    pub(in crate::mount) fn find_file_to_fill(&self, node: LocalItemId, new_size: Option<u64>) -> Result<FileToFill, Errno> {
        self.access_mode_switch.check_read_write()?;
        let row = self.read_connections.read(|reader| reader.find_item(node)).map_err(|error| {
            tracing::warn!("the row of a file to be written could not be read: {error}");
            Errno::EIO
        })?;
        decide_file_to_fill(&row.ok_or(Errno::ESTALE)?, new_size)
    }

    /// The file `node` as the folder shows it: `ESTALE` for a row that is gone and an
    /// item the folder does not show, `EISDIR` for a directory.
    fn read_shown_file(&self, node: LocalItemId) -> Result<ShownFile, Errno> {
        let read = self.run_store_job("the row of a file to be written could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let Some(row) = reader.find_item(node)? else { return Ok(None) };
            Ok(reader.find_path_with_marks(node)?.map(|(path, directory_marks)| (row, path, directory_marks)))
        })?;
        let (row, path, directory_marks) = read.ok_or(Errno::ESTALE)?;
        let local = row.local.clone().ok_or(Errno::ESTALE)?;
        match (row.kind, local.place, local.content) {
            (ItemKind::File, Some(place), Some(content)) => Ok(ShownFile { row, place, content, path, directory_marks }),
            _ => Err(Errno::EISDIR),
        }
    }

    /// Opens the backing file at `path`, which its row says is on disk, to be written: a
    /// downloaded copy is given its write bit first. On this thread, under the tree lock,
    /// no place change runs beside it.
    fn open_backing_file(&self, path: &Path) -> Result<File, Errno> {
        // A file with no write bit to give fails at the open, with the errno to answer.
        let _ = self.backing_directory.grant_write_bit(path);
        self.backing_directory.open_to_write(path).map_err(|error| to_errno(&error))
    }

    /// Makes the backing file of `file`, whose content is not on disk, with `change`
    /// written first: the record (`MakeEntry { owner: Item }`), a place change begun, the
    /// row (`change` and `OnDisk` in one transaction), the file made through the place
    /// change, its marks, the record taken by itself, the change ended. A row refused:
    /// nothing was made, the record is taken, the errno answered. A file that cannot be
    /// made after its row: the mount's take-down, with the record standing.
    fn make_backing_file(&self, file: &ShownFile, change: LocalVersionChange, made_as: MadeFileContent) -> Result<File, Errno> {
        let record = self.write_place_change_record(PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: file.place.clone(), owner: MadeEntryOwner::Item(file.row.local_item_id) })?;
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::RecordWritten);
        let place_change = self.place_changes.begin();
        if let Err(errno) = self.run_store_job("the row of a file that is to be written could not be written", move |store| store.item_store().apply_with_content_on_disk(&change)) {
            self.forget_place_change_record(record);
            return Err(errno);
        }
        self.wake_plan();
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::RowWritten);
        let made = match place_change.make_entry(&file.path, EntryKind::File, &file.directory_marks) {
            Ok((Some(made), _)) => made,
            refused => {
                tracing::error!("the backing file {} could not be made after its row was written ({:?}); the mount is taken down", file.path.display(), refused.err());
                self.mount_take_down.ask(TakeDownCause::BackingFileBehindRow { path: file.path.clone() });
                return Err(Errno::EIO);
            }
        };
        if let Err(error) = self.mark_made_file(&made, file, made_as) {
            tracing::warn!("the backing file {} could not be marked: {error}; the check at start marks it", file.path.display());
        }
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::DiskChanged);
        self.forget_place_change_record(record);
        place_change.end();
        Ok(made)
    }

    /// The marks `made`, the backing file made for `file`, carries as `made_as` says.
    /// Nothing for a file with no binding.
    fn mark_made_file(&self, made: &File, file: &ShownFile, made_as: MadeFileContent) -> std::io::Result<()> {
        let Some(binding) = &file.row.onedrive_item else { return Ok(()) };
        placeholder::write_item_id(made, binding.item_id.as_str())?;
        if made_as == MadeFileContent::ChangedHere {
            return Ok(());
        }
        placeholder::write_state(made, State::Hydrated)?;
        if let ContentVersion::Hashed { ctag: Some(ctag), .. } = &file.content {
            placeholder::write_ctag(made, ctag)?;
        }
        let (mtime, mtime_nsec) = file.content.mtime();
        placeholder::set_mtime(made, to_system_time(mtime, mtime_nsec))
    }

    /// A writer of `node` opened: the row first, then the backing file, then the handle.
    /// `is_overwritten`: the file is to be cut to nothing, and nothing of it is needed.
    fn open_writer(&self, node: LocalItemId, is_overwritten: bool) -> Result<OpenedWriter, Errno> {
        self.access_mode_switch.check_read_write()?;
        let file = self.read_shown_file(node)?;
        let rank = match self.item_handles.is_open_for_writing(node) {
            true => WriterRank::Later,
            false => WriterRank::First,
        };
        let backing_file = match decide_open_change(rank, file.row.content_presence) {
            OpenChange::Nothing => self.open_backing_file(&file.path)?,
            OpenChange::OpenedForWriting => {
                self.run_store_job("the open of a file for writing could not be written", move |store| store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: node }))?;
                self.wake_plan();
                match self.open_backing_file(&file.path) {
                    Ok(backing_file) => backing_file,
                    Err(errno) => {
                        // No writer came of it: the row has the content of before the open again.
                        let _ = self.run_store_job("the open of a file that could not be opened could not be taken back", move |store| store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: node }));
                        return Err(errno);
                    }
                }
            }
            // Its download did not land.
            OpenChange::OpenedForWritingAndOnDisk if !is_overwritten && file.content.size() > 0 => return Err(Errno::ESTALE),
            OpenChange::OpenedForWritingAndOnDisk => {
                let made_as = match is_overwritten {
                    true => MadeFileContent::ChangedHere,
                    false => MadeFileContent::EmptyCopy,
                };
                self.make_backing_file(&file, LocalVersionChange::OpenedForWriting { item: node }, made_as)?
            }
        };
        let backing_file = Arc::new(backing_file);
        let (fh, _) = self.item_handles.add_writer(node, Arc::clone(&backing_file));
        Ok(OpenedWriter { fh, backing_file })
    }

    /// Open for writing (no `O_TRUNC`): one read job (the row and its path);
    /// [`decide_open_change`]; for `OpenedForWritingAndOnDisk` the record
    /// (`MakeEntry { owner: Item(node) }`) and a place change begun; the row; then the
    /// backing file made, or opened, the record taken by itself, and the writer added
    /// to `ItemHandles`. A file that is still not on disk and not empty is `ESTALE`: its
    /// download did not land. A file on disk that cannot be opened after its row: the row
    /// is given the content of before the open again. Called on the write-operation
    /// thread.
    pub(in crate::mount) fn open_to_write(&self, node: LocalItemId) -> Result<OpenedWriter, Errno> {
        self.open_writer(node, false)
    }

    /// Open with `O_TRUNC`: as [`open_to_write`](Self::open_to_write) with nothing
    /// downloaded, then the backing file cut to nothing and the handle marked written.
    pub(in crate::mount) fn open_to_overwrite(&self, node: LocalItemId) -> Result<OpenedWriter, Errno> {
        let opened = self.open_writer(node, true)?;
        if let Err(error) = opened.backing_file.set_len(0) {
            self.close_writer(opened.fh);
            return Err(to_errno(&error));
        }
        // Cut to nothing: its content is the user's own from here on.
        self.item_handles.mark_written(node);
        Ok(opened)
    }

    /// The writer's file a request about `node` with `fh` is answered from, with the item
    /// it writes: the handle's, when `fh` is a writer's — also for a node whose row is
    /// gone (`docs/design/mount.md` §4.1) — else that of any writer of `node`.
    fn find_writer(&self, node: LocalItemId, fh: Option<Fh>) -> Option<(LocalItemId, Arc<File>)> {
        fh.and_then(|fh| self.item_handles.find_file_to_write(fh)).or_else(|| self.item_handles.find_writer_file(node).map(|backing_file| (node, backing_file)))
    }

    /// The row says what the file at `path` does not hold, and the file could not be made
    /// to: the mount's take-down, and `EIO`.
    fn take_down_for_file(&self, path: &Path, error: &std::io::Error) -> Errno {
        tracing::error!("the backing file {} could not be changed after its row was written: {error}; the mount is taken down", path.display());
        self.mount_take_down.ask(TakeDownCause::BackingFileBehindRow { path: path.to_owned() });
        Errno::EIO
    }

    /// Truncate (setattr with a size). With a writer open (found by the item, or `fh`
    /// given): the backing file is cut and the handle marked written; no row. With none:
    /// the row first — `Truncated { item, size, mtime, mtime_nsec }`, with `OnDisk` in the
    /// same transaction for a file that was not on disk and is cut to nothing, whose
    /// backing file is then made: on record before that row (`MakeEntry { owner:
    /// Item(node) }`), the record taken by itself once the file is made — then the
    /// file is cut and given the time the row was given. A node whose row is gone, asked
    /// with the `fh` of a writer: the handle's file is cut, and no row written
    /// (`docs/design/mount.md` §4.1). A file on disk is opened before its row is written, so that one that cannot
    /// be opened writes none; a file that cannot be cut after its row is the mount's
    /// take-down. The attributes after it are the caller's to read, as getattr does.
    pub(in crate::mount) fn truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        if let Some((written, backing_file)) = self.find_writer(node, fh) {
            backing_file.set_len(size).map_err(|error| to_errno(&error))?;
            self.item_handles.mark_written(written);
            return Ok(());
        }
        let file = self.read_shown_file(node)?;
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let (mtime, mtime_nsec) = (now.as_secs() as i64, now.subsec_nanos());
        let change = LocalVersionChange::Truncated { item: node, size, mtime, mtime_nsec };
        let backing_file = match file.row.content_presence {
            ContentPresence::OnDisk => {
                let backing_file = self.open_backing_file(&file.path)?;
                self.run_store_job("a truncation could not be written", move |store| store.item_store().apply(&change))?;
                self.wake_plan();
                backing_file
            }
            // Its download did not land.
            ContentPresence::NotOnDisk if file.content.size() > 0 && size > 0 => return Err(Errno::ESTALE),
            ContentPresence::NotOnDisk => self.make_backing_file(&file, change, MadeFileContent::ChangedHere)?,
        };
        backing_file.set_len(size).and_then(|()| placeholder::set_mtime(&backing_file, to_system_time(mtime, mtime_nsec))).map_err(|error| self.take_down_for_file(&file.path, &error))
    }

    /// Setattr with a mode and neither a size nor a time (chmod): the owner's executable
    /// bit of a file goes to its row (`ItemStore::set_executable_bit`) where the row does
    /// not say so already; nothing else of a mode is kept, and nothing is unsent by it.
    /// Nothing is written for a directory, nor for a node whose row is gone: the answer is
    /// `Ok` all the same, as it was while no mode was kept. The attributes after it are
    /// the caller's to read.
    pub(in crate::mount) fn set_executable_bit(&self, node: LocalItemId, is_executable: bool) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        self.run_store_job("the executable bit of a file could not be written", move |store| {
            let mut items = store.item_store();
            match items.find_item(node)? {
                Some(row) if row.kind == ItemKind::File && row.is_executable != is_executable => items.set_executable_bit(node, is_executable),
                Some(_) | None => Ok(()),
            }
        })
    }

    /// Setattr with a time and no size: the time goes to the backing file where the file
    /// has one, then to the row (`ItemStore::set_local_modified_time`); with a writer open
    /// the backing file alone, and the handle marked written, so that the last close
    /// writes the time. A node whose row is gone, asked with an `fh`: the handle's file
    /// alone. A row that refuses the time: the backing file gets the time it had. A
    /// directory's row keeps no time: the time goes to its directory in the backing
    /// directory, when it is there; for a directory that is not on disk nothing changes
    /// and the answer is `Ok` all the same (its time is the mount's start, as getattr
    /// says). The attributes after it are the caller's to read.
    pub(in crate::mount) fn set_modified_time(&self, node: LocalItemId, fh: Option<Fh>, mtime: i64, mtime_nsec: u32) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let time = to_system_time(mtime, mtime_nsec);
        if let Some((written, backing_file)) = self.find_writer(node, fh) {
            placeholder::set_mtime(&backing_file, time).map_err(|error| to_errno(&error))?;
            self.item_handles.mark_written(written);
            return Ok(());
        }
        let read = self.run_store_job("the row of an item whose time is set could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let Some(row) = reader.find_item(node)? else { return Ok(None) };
            Ok(reader.find_path(node)?.map(|path| (row, path)))
        })?;
        let (row, path) = read.ok_or(Errno::ESTALE)?;
        let on_disk = match (row.kind, row.content_presence) {
            (ItemKind::File, ContentPresence::NotOnDisk) => None,
            (ItemKind::File, ContentPresence::OnDisk) => Some(self.backing_directory.open_to_read(&path).map_err(|error| to_errno(&error))?),
            (ItemKind::Directory, _) => self.backing_directory.open_to_read(&path).ok(),
        };
        let time_before = match &on_disk {
            Some(on_disk) => {
                let time_before = on_disk.metadata().and_then(|metadata| metadata.modified()).map_err(|error| to_errno(&error))?;
                placeholder::set_mtime(on_disk, time).map_err(|error| to_errno(&error))?;
                Some(time_before)
            }
            None => None,
        };
        if row.kind == ItemKind::Directory {
            return Ok(());
        }
        if let Err(errno) = self.run_store_job("a time set on a file could not be written", move |store| store.item_store().set_local_modified_time(node, mtime, mtime_nsec)) {
            if let (Some(on_disk), Some(time_before)) = (&on_disk, time_before) {
                let _ = placeholder::set_mtime(on_disk, time_before);
            }
            return Err(errno);
        }
        self.wake_plan();
        Ok(())
    }

    /// Release of a writer, in this order: `ItemHandles::find_closing_writer` (the writer
    /// stays among the handles); at the last close the size and the times read from the
    /// backing file through the handle and [`decide_close_change`]'s answer written; then
    /// `ItemHandles::remove_writer`; then the plan woken. So getattr finds the writer's
    /// file until the row has the new size. The program was answered already: a failure
    /// is logged, and the check at start mends the row. Called on the write-operation
    /// thread; a job dropped at the mount's stop leaves `content_before_write` set, which
    /// the check at start reads (`docs/design/mount.md` §7.2). Written whatever the access
    /// mode is by now.
    pub(in crate::mount) fn close_writer(&self, fh: Fh) {
        let Some(closing) = self.item_handles.find_closing_writer(fh) else { return };
        if closing.outcome == CloseOutcome::StillOpenElsewhere {
            self.item_handles.remove_writer(fh);
            return;
        }
        match read_size_and_time(&closing.backing_file) {
            Ok((size, mtime, mtime_nsec)) => {
                let closed = closing.clone();
                let _ = self.run_store_job("the last close of a file could not be written; the check at start mends its row", move |store| {
                    let mut items = store.item_store();
                    let is_shown = items.reader().find_item(closed.item)?.is_some_and(|row| row.local.is_some());
                    match decide_close_change(&closed, is_shown, size, mtime, mtime_nsec) {
                        Some(change) => items.apply(&change).map(drop),
                        None => Ok(()),
                    }
                });
            }
            Err(error) => tracing::warn!("the backing file of item {} could not be looked at when its last writer closed it: {error}; the check at start mends its row", closing.item),
        }
        self.item_handles.remove_writer(fh);
        self.wake_plan();
    }
}
