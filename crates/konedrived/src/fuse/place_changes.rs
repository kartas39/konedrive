//! Every change of a place in the backing directory, and every open of a backing file by
//! the path of a row: the one part through which the disk's names change and are read
//! while the mount is up.
//!
//! **State owned:** the count of place changes: raised when a change of the backing
//! directory's names begins — before the disk is touched, and for a content operation
//! that makes a backing file before its row is written — and raised again when it ended —
//! after its row was written and the disk has what the row says, or after it was undone.
//! Odd while one is under way. Raised by [`PlaceChange`], which only
//! [`PlaceChanges::begin`] hands out and which is the only thing that renames, makes,
//! links or sets aside an entry: a change of the disk's names that does not raise the
//! count cannot be written. Nothing else of `fuse` calls `BackingDirectory`'s `rename`,
//! `make_*`, `land` or `remove_*` once the switch is made.
//!
//! **Answers for:**
//!
//! - *An open by path never gives a node another item's file.* The disk changes BEFORE
//!   the row (design §8), so a reader that reads a row, takes its path and opens it could
//!   meet one rename (a missing file) or two (another item's file). Every such reader
//!   goes through [`PlaceChanges::open_at_row_path`]: the count is read (and waited for
//!   while odd), then the row and its path in ONE read transaction, then the file is
//!   opened, then the count is read again; when it moved, what was opened is dropped and
//!   the whole is tried again. What is answered was opened at a moment at which the rows
//!   and the disk said the same of that path.
//! - *The retry ends.* After [`CROSSED_TRIES`] tries that a change crossed, the reader
//!   takes the lock `changes` for its next try, and [`PlaceChanges::begin`] takes that
//!   lock before it raises the count: the next change waits for one read and one open,
//!   and the reader's try cannot be crossed. Under a stream of changes (`tar x`, `rm -r`,
//!   a first listing's moves) a reader gets its file at the fourth try at the latest.
//! - *A stat never sleeps.* [`PlaceChanges::stat_at_row_path`] tries once more when the
//!   count moved and then answers no facts: a lookup or a getattr of a file on disk waits
//!   for no place change, and so for no job of the store behind one. A crossed stat costs
//!   the blocks of one answer.
//! - *One shape of "change a place, keep the undo, set aside"* for the mount's write
//!   operations, the steps towards the folder and the landing of a download: the name a
//!   set-aside entry gets, the directories made above a place and their marks, and
//!   nothing replaced that the rows do not say (the map's E2, E21).
//!
//! **The names of entries set aside.** An entry is set aside under
//! `folder::disk::make_aside_entry_name` of its item, in the root: an entry no row says
//! (`is_rowless_entry_name`), which the mount hides and the check at start skips. It is
//! removed for good by [`PlaceChanges::discard`], after its change ended; a leftover of a
//! death is removed by [`PlaceChanges::remove_aside_entries`], which the mount's build
//! calls before the mount goes up: this part is the one owner of those entries. A
//! temporary name of an exchange is NOT one of them: it is a row's place, moved to by
//! [`PlaceChange::move_entry`] like any other, and the check reads it as that row's.
//!
//! **The order of locks**, for everything of the mount over the rows. A later one may be
//! taken while an earlier is held, never the other way:
//!
//! 1. the folder's lease (the plan's task, around a step towards the folder);
//! 2. the folder's tree lock (each job of the write-operation thread; a step's commit);
//! 3. `changes`, this part's lock: taken by [`PlaceChanges::begin`] to raise the count
//!    (on the write-operation thread alone, so under 2) and let go at once; and taken by
//!    a reader for one whole try after its tries were crossed — a reader that holds none
//!    of 1 and 2, across one read closure (4) and one open;
//! 4. one job of the store's thread (`Store::call_blocking`), or one read closure of a
//!    reading connection — never holding 5;
//! 5. the leaf mutexes, each held for its own few lines and never across 1 to 4 nor
//!    across each other: `ItemHandles`, `RunningSteps`, `PublishedPlan`, `SendingHolds`
//!    (whose setters wake the plan after their own lock is let go), `SyncPlanWake`,
//!    `ItemDownloadStates`, the bridge's queue, the map of `ReadConnections`.
//!
//! A place change is open only inside one job of the write-operation thread, which waits
//! for the store by `call_blocking` and for nothing else: not for a mount thread, the
//! bridge or the kernel (announcements are queued after the end). [`open_at_row_path`]
//! is never called on the write-operation thread: with its own change open it would
//! sleep on itself. Both are asserted in debug builds by the thread's id, which this
//! part is given.
//!
//! [`open_at_row_path`]: PlaceChanges::open_at_row_path
//!
//! **Tested without the rest**, over a temporary backing directory:
// test: the_count_is_odd_from_the_begin_of_a_change_to_its_end
// test: a_reader_that_began_during_a_change_sleeps_until_its_end_and_is_woken
// test: a_reader_whose_open_was_crossed_by_a_change_tries_again_and_gets_the_file_of_the_row
// test: two_renames_between_a_readers_row_and_its_open_never_give_it_the_other_items_file
// test: after_three_crossed_tries_the_next_change_waits_for_the_readers_try
// test: a_stat_that_was_crossed_twice_answers_no_facts_and_does_not_sleep
// test: a_missing_file_at_a_moment_no_change_crossed_is_told_as_no_file
// test: a_move_to_a_place_held_on_disk_is_refused_and_replaces_nothing
// test: a_move_makes_the_directories_above_marked_and_its_undo_removes_those_it_made
// test: a_file_is_linked_under_its_name_with_nothing_replaced
// test: a_directory_made_where_one_is_on_disk_takes_it_and_marks_it_again
// test: an_entry_set_aside_is_put_back_by_its_undo_and_gone_after_its_discard
// test: the_entries_a_death_left_set_aside_are_removed_before_the_mount_goes_up
// test: a_temporary_name_is_moved_to_like_any_place_and_is_not_cleaned
// test: a_change_dropped_without_its_end_still_ends_the_count_and_wakes_the_sleepers
// test: an_undo_that_fails_is_answered_as_such_and_asks_nothing_by_itself
#![allow(dead_code, unused_variables)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use konedrive_tree::{LocalItemId, OneDriveItemId, TreeError};

use super::backing_directory::{BackingDirectory, DiskMetadata, EntryKind};
use crate::folder::step_outcome::StepTrouble;

/// After how many tries that a change crossed a reader takes `changes` for its next try.
pub(super) const CROSSED_TRIES: u32 = 3;

/// How a change of the backing directory is taken back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlaceUndo {
    /// Nothing was changed on disk.
    Nothing,
    /// An entry was made or linked: it is removed again, with the directories made above
    /// it.
    RemoveMade { path: PathBuf, directories_made: Vec<PathBuf> },
    /// A directory that was on disk already was taken as the made one and marked: its
    /// mark is taken off again; the directory stays.
    UnmarkTaken { path: PathBuf },
    /// An entry was moved: back, and the directories made above the new place removed.
    MoveBack { from: PathBuf, to: PathBuf, directories_made: Vec<PathBuf> },
    /// An entry was moved over another, which was set aside first: the first back, the
    /// second put back under its name.
    MoveBackAndRestore { from: PathBuf, to: PathBuf, set_aside: PathBuf },
    /// An entry was set aside under its item's aside name: back under its name.
    Restore { set_aside: PathBuf, path: PathBuf },
}

/// Why a change of the backing directory was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlaceRefusal {
    /// The place is held on disk by what the rows do not say (`EEXIST` to a program; a
    /// step ends `StepObstacle::PlaceHeldOnDisk`).
    PlaceHeld { path: PathBuf },
    /// The disk refused: the errno.
    Disk { errno: i32, path: PathBuf },
}

/// What a directory made above a place is marked with: the item id of its binding, for
/// each directory of the path that has one, the outermost first
/// (`ItemReader::find_path_with_marks`).
pub(super) type DirectoryMarks = Vec<Option<OneDriveItemId>>;

/// How a backing file is opened by a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenMode {
    Read,
    Write,
}

/// Why an open at a row's path failed.
#[derive(Debug)]
pub(super) enum OpenAtRowError {
    /// The store failed under the read closure.
    Store(TreeError),
    /// The row says the path, and at a moment no change crossed the disk has nothing
    /// there: a row that says on disk over a missing file (the map's E1).
    NoFile { path: PathBuf },
    /// Any other error of the disk.
    Disk(std::io::Error),
}

/// The errno a program gets for `error`: `EIO` for the store, the disk's own otherwise;
/// `ENOENT` for no file, which the open request first tries to mend
/// (`ItemFreeUp::mend_missing_copy`). Pure.
pub(super) fn decide_open_errno(error: &OpenAtRowError) -> i32 {
    todo!()
}

/// What a step that opens a file by its row answers for `error`: no file is
/// `StepObstacle::BackingFileMissing`, anything else a failure. Pure.
pub(super) fn decide_open_trouble(error: &OpenAtRowError) -> StepTrouble {
    todo!()
}

/// The place changes of one mount.
pub(super) struct PlaceChanges {
    backing_directory: Arc<BackingDirectory>,
    count: AtomicU64,
    /// Lock 3 of the order: held by `begin` while it raises the count, and by a reader
    /// for one whole try after `CROSSED_TRIES` crossed ones; also what a reader that met a
    /// change under way sleeps under, with `ended` — it reads the count again under it
    /// before it waits, and an end notifies under it, so that no wake is lost.
    changes: std::sync::Mutex<()>,
    /// Told at every end of a place change.
    ended: std::sync::Condvar,
    /// The write-operation thread's id, for the two debug assertions.
    write_operation_thread_id: std::thread::ThreadId,
}

/// One change of the backing directory's names, from before the disk is touched until its
/// row is written or it is undone. Dropping it ends it.
pub(super) struct PlaceChange<'a> {
    place_changes: &'a PlaceChanges,
}

impl PlaceChanges {
    /// The place changes of `backing_directory`; none is under way. Made by the mount's
    /// build, after the write-operation thread, before anything that changes or opens by
    /// path.
    pub(super) fn new(backing_directory: Arc<BackingDirectory>, write_operation_thread_id: std::thread::ThreadId) -> Self {
        todo!()
    }

    /// Removes every entry of the root that was set aside and never discarded
    /// (`folder::disk::ASIDE_ENTRY_PREFIX`): what a death between a set-aside and its
    /// discard, or a failed discard, left. How many. Called by the mount's build before
    /// the check at start, which skips such names: nothing of them is ever read as a file
    /// made by hand.
    pub(super) fn remove_aside_entries(&self) -> std::io::Result<usize> {
        todo!()
    }

    /// A place change begins: `changes` is taken, the count raised, `changes` let go; the
    /// count is odd from now until the answer is dropped. Called on the write-operation
    /// thread alone (asserted), under the tree lock, before the disk is touched — and by
    /// a content operation that makes a backing file, before its row is written; never
    /// while another is under way.
    pub(super) fn begin(&self) -> PlaceChange<'_> {
        todo!()
    }

    /// Opens the backing file at the path `find_path` answers, with what else it answers
    /// (the row, a content version), such that no place change came between the two:
    /// while a change is under way it sleeps on `ended`; then calls `find_path` — which
    /// reads the row and its path in ONE read closure (`ReadConnections::read` or
    /// `Store::read_items_blocking`) — opens, and tries again from the start when the
    /// count moved meanwhile. After `CROSSED_TRIES` crossed tries the next try holds
    /// `changes`, which no change can begin under: it is the last. `None` when
    /// `find_path` answers none. `OpenAtRowError::NoFile` when the disk has nothing there
    /// at a moment no change crossed.
    ///
    /// Called by every open of a backing file outside the write-operation thread: the open
    /// request's read branch, a read whose content landed since, `ItemContentFiles`.
    /// Never on the write-operation thread (asserted).
    pub(super) fn open_at_row_path<T>(&self, mode: OpenMode, find_path: impl FnMut() -> Result<Option<(PathBuf, T)>, TreeError>) -> Result<Option<(File, T)>, OpenAtRowError> {
        todo!()
    }

    /// One `stat` at the path `find_path` answers: the blocks a file takes on disk, a
    /// directory's time. It never sleeps: while a change is under way, or when the count
    /// moved between the read and the stat, it tries once more, and then answers `None` —
    /// no facts, which the attributes are made without.
    pub(super) fn stat_at_row_path<T>(&self, find_path: impl FnMut() -> Result<Option<(PathBuf, T)>, TreeError>) -> Result<Option<(DiskMetadata, T)>, TreeError> {
        todo!()
    }

    /// The row was written and the change ended: removes for good what `undo` kept set
    /// aside. Outside every change: an aside name is no row's path, so no reader needs the
    /// count for it, and the unlink of a large file holds up no opener. A failure is
    /// logged and left for [`remove_aside_entries`](Self::remove_aside_entries).
    pub(super) fn discard(&self, undo: PlaceUndo) {
        todo!()
    }
}

impl PlaceChange<'_> {
    /// Makes a file or a directory at `path`, with the directories above it that are not
    /// on disk, each marked. A file is refused when something is at `path`. A directory
    /// that is on disk already — one a removal left there because it still held what no
    /// row says — is taken as the made one and marked again (`PlaceUndo::UnmarkTaken`).
    pub(super) fn make_entry(&self, path: &Path, kind: EntryKind, directory_marks: &DirectoryMarks) -> Result<(Option<File>, PlaceUndo), PlaceRefusal> {
        todo!()
    }

    /// Links the unnamed file `file` under `path`, with the directories above it that are
    /// not on disk made and marked, and syncs the directory. Nothing is replaced:
    /// `PlaceRefusal::PlaceHeld` when something is there. Undo: `RemoveMade`. What a
    /// landing of a download names its file by.
    pub(super) fn link_file(&self, file: &File, path: &Path, directory_marks: &DirectoryMarks) -> Result<PlaceUndo, PlaceRefusal> {
        todo!()
    }

    /// Moves the entry at `from` to `to`, where the rows say nothing is: with
    /// `RENAME_NOREPLACE`, after making the directories above `to` that are not on disk,
    /// each marked. `PlaceRefusal::PlaceHeld` when the disk holds something there. A
    /// temporary name of an exchange is a `to` like any other.
    pub(super) fn move_entry(&self, from: &Path, to: &Path, directory_marks: &DirectoryMarks) -> Result<PlaceUndo, PlaceRefusal> {
        todo!()
    }

    /// Moves the entry at `from` over the entry at `to`, which the rows say is there and
    /// is the entry of `replaced`: that one is set aside first.
    pub(super) fn move_entry_over(&self, from: &Path, to: &Path, replaced: LocalItemId) -> Result<PlaceUndo, PlaceRefusal> {
        todo!()
    }

    /// Sets the entry at `path`, the entry of `item`, aside under
    /// `folder::disk::make_aside_entry_name(item)` in the root: what a removal does first.
    /// `PlaceUndo::Nothing` when nothing is at `path`. A directory that is not empty on
    /// disk — it holds what no row says — is left where it is, with its mark taken off,
    /// and is `Nothing` too: a later directory made at that name takes it
    /// ([`make_entry`](Self::make_entry)), and nothing in it is ever removed by the
    /// daemon.
    pub(super) fn set_aside(&self, path: &Path, item: LocalItemId) -> Result<PlaceUndo, PlaceRefusal> {
        todo!()
    }

    /// Takes `undo` back. An error is the disk's: the caller asks for the mount's
    /// take-down (`MountTakeDown::ask`).
    pub(super) fn undo(&self, undo: PlaceUndo) -> std::io::Result<()> {
        todo!()
    }

    /// The change ended: its row is written and the disk has what the row says, or it was
    /// undone. The count is even again, and who sleeps on `ended` is woken, under
    /// `changes`. The same as dropping it; written out where the end must be seen.
    pub(super) fn end(self) {
        todo!()
    }
}
