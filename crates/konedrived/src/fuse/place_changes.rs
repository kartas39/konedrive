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

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, MutexGuard, PoisonError};

use konedrive_tree::{LocalItemId, OneDriveItemId, TreeError};
use nix::fcntl::{renameat2, RenameFlags, AT_FDCWD};

use super::backing_directory::{BackingDirectory, DiskMetadata, EntryKind};
use crate::folder::disk::{make_aside_entry_name, ASIDE_ENTRY_PREFIX};
use crate::folder::step_outcome::{StepFailure, StepObstacle, StepTrouble};

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
    match error {
        OpenAtRowError::Store(_) => libc::EIO,
        OpenAtRowError::NoFile { .. } => libc::ENOENT,
        OpenAtRowError::Disk(error) => error.raw_os_error().unwrap_or(libc::EIO),
    }
}

/// What a step that opens a file by its row answers for `error`: no file is
/// `StepObstacle::BackingFileMissing`, anything else a failure. Pure. A failure of the
/// disk is told with no path: the error carries none.
pub(super) fn decide_open_trouble(error: &OpenAtRowError) -> StepTrouble {
    match error {
        OpenAtRowError::NoFile { .. } => StepTrouble::Obstacle(StepObstacle::BackingFileMissing),
        OpenAtRowError::Store(error) => StepTrouble::Failure(StepFailure::Store { detail: error.to_string() }),
        OpenAtRowError::Disk(error) => StepTrouble::Failure(StepFailure::Disk { errno: error.raw_os_error().unwrap_or(libc::EIO), path: PathBuf::new() }),
    }
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
    /// How many readers sleep on `ended` now. Raised and lowered under `changes`, so a
    /// reader counted here has let `changes` go: nothing decides by it, a test waits for
    /// it in place of a sleep.
    sleeper_count: AtomicUsize,
}

/// One change of the backing directory's names, from before the disk is touched until its
/// row is written or it is undone. Dropping it ends it.
pub(super) struct PlaceChange<'a> {
    place_changes: &'a PlaceChanges,
}

/// What is at a path that is to be set aside.
enum EntryToSetAside {
    Nothing,
    /// A file, or a directory with nothing in it: it is moved to its aside name.
    Movable,
    /// A directory that holds what no row says: it stays where it is.
    HeldDirectory,
}

fn to_refusal(error: &io::Error, path: &Path) -> PlaceRefusal {
    PlaceRefusal::Disk { errno: error.raw_os_error().unwrap_or(libc::EIO), path: path.to_owned() }
}

impl PlaceChanges {
    /// The place changes of `backing_directory`; none is under way. Made by the mount's
    /// build, after the write-operation thread, before anything that changes or opens by
    /// path.
    pub(super) fn new(backing_directory: Arc<BackingDirectory>, write_operation_thread_id: std::thread::ThreadId) -> Self {
        PlaceChanges { backing_directory, count: AtomicU64::new(0), changes: std::sync::Mutex::new(()), ended: std::sync::Condvar::new(), write_operation_thread_id, sleeper_count: AtomicUsize::new(0) }
    }

    /// Removes every entry of the root that was set aside and never discarded
    /// (`folder::disk::ASIDE_ENTRY_PREFIX`): what a death between a set-aside and its
    /// discard, or a failed discard, left. How many. Called by the mount's build before
    /// the check at start, which skips such names: nothing of them is ever read as a file
    /// made by hand.
    pub(super) fn remove_aside_entries(&self) -> std::io::Result<usize> {
        let mut removed = 0;
        for entry in self.backing_directory.read_directory(Path::new(""))? {
            let name = entry?.file_name();
            if name.as_bytes().starts_with(ASIDE_ENTRY_PREFIX.as_bytes()) {
                self.remove_aside_entry(Path::new(&name))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// A place change begins: `changes` is taken, the count raised, `changes` let go; the
    /// count is odd from now until the answer is dropped. Called on the write-operation
    /// thread alone (asserted), under the tree lock, before the disk is touched — and by
    /// a content operation that makes a backing file, before its row is written; never
    /// while another is under way.
    pub(super) fn begin(&self) -> PlaceChange<'_> {
        debug_assert_eq!(std::thread::current().id(), self.write_operation_thread_id, "a place change begins on the write-operation thread alone");
        let held = crate::panic::lock(&self.changes);
        debug_assert_eq!(self.count.load(Ordering::SeqCst) % 2, 0, "a place change begins while none is under way");
        self.count.fetch_add(1, Ordering::SeqCst);
        drop(held);
        PlaceChange { place_changes: self }
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
    pub(super) fn open_at_row_path<T>(&self, mode: OpenMode, mut find_path: impl FnMut() -> Result<Option<(PathBuf, T)>, TreeError>) -> Result<Option<(File, T)>, OpenAtRowError> {
        debug_assert_ne!(std::thread::current().id(), self.write_operation_thread_id, "a backing file is opened by a row's path off the write-operation thread, which would sleep on its own change");
        let mut crossed_tries = 0;
        loop {
            // The last try holds `changes` from before its read until after its open: no
            // change begins under it.
            let (count, held) = match crossed_tries >= CROSSED_TRIES {
                true => {
                    let (count, held) = self.sleep_until_no_change(crate::panic::lock(&self.changes));
                    (count, Some(held))
                }
                false => (self.wait_for_no_change(), None),
            };
            let Some((path, row)) = find_path().map_err(OpenAtRowError::Store)? else { return Ok(None) };
            let opened = self.open(mode, &path);
            if self.count.load(Ordering::SeqCst) != count {
                // A change began since the count was read: what was opened may be another
                // item's file, and a missing one may only have moved.
                crossed_tries += 1;
                continue;
            }
            drop(held);
            return match opened {
                Ok(file) => Ok(Some((file, row))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Err(OpenAtRowError::NoFile { path }),
                Err(error) => Err(OpenAtRowError::Disk(error)),
            };
        }
    }

    /// One `stat` at the path `find_path` answers: the blocks a file takes on disk, a
    /// directory's time. It never sleeps: while a change is under way, or when the count
    /// moved between the read and the stat, it tries once more, and then answers `None` —
    /// no facts, which the attributes are made without.
    pub(super) fn stat_at_row_path<T>(&self, mut find_path: impl FnMut() -> Result<Option<(PathBuf, T)>, TreeError>) -> Result<Option<(DiskMetadata, T)>, TreeError> {
        for _try in 0..2 {
            let count = self.count.load(Ordering::SeqCst);
            if count % 2 == 1 {
                // A change is under way. Not waited for: the other try comes at once.
                std::thread::yield_now();
                continue;
            }
            let Some((path, row)) = find_path()? else { return Ok(None) };
            let facts = self.read_facts(&path);
            if self.count.load(Ordering::SeqCst) == count {
                return Ok(facts.map(|facts| (facts, row)));
            }
        }
        Ok(None)
    }

    /// The row was written and the change ended: removes for good what `undo` kept set
    /// aside. Outside every change: an aside name is no row's path, so no reader needs the
    /// count for it, and the unlink of a large file holds up no opener. A failure is
    /// logged and left for [`remove_aside_entries`](Self::remove_aside_entries).
    pub(super) fn discard(&self, undo: PlaceUndo) {
        let (PlaceUndo::Restore { set_aside, .. } | PlaceUndo::MoveBackAndRestore { set_aside, .. }) = undo else { return };
        if let Err(error) = self.remove_aside_entry(&set_aside) {
            tracing::warn!("cannot remove {} from the backing directory, which was set aside: {error}; it goes at the next start", set_aside.display());
        }
    }

    /// The count at a moment at which no change is under way: sleeps on `ended` while one
    /// is. `changes` is taken only to sleep.
    fn wait_for_no_change(&self) -> u64 {
        let count = self.count.load(Ordering::SeqCst);
        if count % 2 == 0 {
            return count;
        }
        self.sleep_until_no_change(crate::panic::lock(&self.changes)).0
    }

    /// Sleeps on `ended`, under `held`, until no change is under way: the count then, and
    /// the lock, still held. The count is read under the lock before each wait, and an
    /// end raises it and notifies under the lock: no wake is lost.
    fn sleep_until_no_change<'a>(&self, mut held: MutexGuard<'a, ()>) -> (u64, MutexGuard<'a, ()>) {
        loop {
            let count = self.count.load(Ordering::SeqCst);
            if count % 2 == 0 {
                return (count, held);
            }
            self.sleeper_count.fetch_add(1, Ordering::SeqCst);
            held = self.ended.wait(held).unwrap_or_else(PoisonError::into_inner);
            self.sleeper_count.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn open(&self, mode: OpenMode, path: &Path) -> io::Result<File> {
        match mode {
            OpenMode::Read => self.backing_directory.open_to_read(path),
            OpenMode::Write => self.backing_directory.open_to_write(path),
        }
    }

    /// What the disk says of the file or directory at `path`; `None` when nothing is
    /// there, and for what is neither.
    fn read_facts(&self, path: &Path) -> Option<DiskMetadata> {
        DiskMetadata::from_filesystem(&self.backing_directory.read_metadata(path)?)
    }

    fn is_directory(&self, path: &Path) -> bool {
        self.backing_directory.read_metadata(path).is_some_and(|metadata| metadata.is_dir())
    }

    /// Removes the entry at the aside name `aside`, with what is below it.
    fn remove_aside_entry(&self, aside: &Path) -> io::Result<()> {
        match self.is_directory(aside) {
            true => std::fs::remove_dir_all(self.backing_directory.resolve_path(aside)),
            false => self.backing_directory.remove_file(aside),
        }
    }

    /// Moves the entry at `from` to `to`, never over anything (`RENAME_NOREPLACE`).
    fn rename_without_replacing(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (self.backing_directory.resolve_path(from), self.backing_directory.resolve_path(to));
        renameat2(AT_FDCWD, &from, AT_FDCWD, &to, RenameFlags::RENAME_NOREPLACE).map_err(io::Error::from)
    }

    /// Makes the directories above `path` that are not on disk, and marks each directory
    /// above `path` with the id `directory_marks` has for it. The directories it made, the
    /// outermost first. When it is refused, what it made is removed again.
    fn make_directories_above(&self, path: &Path, directory_marks: &DirectoryMarks) -> Result<Vec<PathBuf>, PlaceRefusal> {
        let mut directories_made = Vec::new();
        let mut directory = PathBuf::new();
        for (index, name) in path.parent().unwrap_or(Path::new("")).components().enumerate() {
            directory.push(name);
            let made = match self.backing_directory.make_directory(&directory) {
                Ok(()) => {
                    directories_made.push(directory.clone());
                    Ok(())
                }
                // What is there must be a directory: a file in the way is the user's own.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match self.is_directory(&directory) {
                    true => Ok(()),
                    false => Err(PlaceRefusal::PlaceHeld { path: directory.clone() }),
                },
                Err(error) => Err(to_refusal(&error, &directory)),
            };
            if let Err(refusal) = made.and_then(|()| self.mark_directory(&directory, directory_marks.get(index))) {
                self.remove_directories(&directories_made);
                return Err(refusal);
            }
        }
        Ok(directories_made)
    }

    /// Marks the directory at `directory` with `mark`, when it has one and is not marked
    /// with it already.
    fn mark_directory(&self, directory: &Path, mark: Option<&Option<OneDriveItemId>>) -> Result<(), PlaceRefusal> {
        let Some(Some(mark)) = mark else { return Ok(()) };
        if self.backing_directory.read_onedrive_id(directory).as_deref() == Some(mark.as_str()) {
            return Ok(());
        }
        self.backing_directory.mark_with_onedrive_id(directory, mark.as_str()).map_err(|error| to_refusal(&error, directory))
    }

    /// Removes `directories`, the innermost first: what a change made above its place,
    /// after it was refused. What cannot be removed is left and logged.
    fn remove_directories(&self, directories: &[PathBuf]) {
        if let Err(error) = self.try_remove_directories(directories) {
            tracing::warn!("cannot remove a directory made in the backing directory for a change that was refused: {error}");
        }
    }

    fn try_remove_directories(&self, directories: &[PathBuf]) -> io::Result<()> {
        directories.iter().rev().try_for_each(|directory| self.backing_directory.remove_directory(directory))
    }

    /// Syncs the directory at `directory` (`""` is the backing directory itself): a name
    /// made in it is on disk for good.
    fn sync_directory(&self, directory: &Path) -> io::Result<()> {
        File::open(self.backing_directory.resolve_path(directory))?.sync_all()
    }

    fn inspect_entry_to_set_aside(&self, path: &Path) -> io::Result<EntryToSetAside> {
        let Some(metadata) = self.backing_directory.read_metadata(path) else { return Ok(EntryToSetAside::Nothing) };
        if metadata.is_dir() && self.backing_directory.read_directory(path)?.next().is_some() {
            return Ok(EntryToSetAside::HeldDirectory);
        }
        Ok(EntryToSetAside::Movable)
    }

    /// Moves the entry at `path` to the aside name `aside`. What a failed discard left
    /// under that name gives way: one item has one entry set aside at most.
    fn move_aside(&self, path: &Path, aside: &Path) -> io::Result<()> {
        match self.rename_without_replacing(path, aside) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.remove_aside_entry(aside)?;
                self.rename_without_replacing(path, aside)
            }
            moved => moved,
        }
    }
}

impl PlaceChange<'_> {
    /// Makes a file or a directory at `path`, with the directories above it that are not
    /// on disk, each marked. A file is refused when something is at `path`. A directory
    /// that is on disk already — one a removal left there because it still held what no
    /// row says — is taken as the made one and marked again (`PlaceUndo::UnmarkTaken`).
    ///
    /// `directory_marks` is read against the names of `path`, the outermost first: its
    /// first mark is of the first name, and so on. Marks for the directories above `path`
    /// are enough (`find_path_with_marks` of the directory the entry is made in); a mark
    /// for the last name, when there is one, is the made directory's own
    /// (`find_path_with_marks` of the directory that is made), and is nothing to a file.
    /// A directory above that is on disk already is marked too when it is not.
    pub(super) fn make_entry(&self, path: &Path, kind: EntryKind, directory_marks: &DirectoryMarks) -> Result<(Option<File>, PlaceUndo), PlaceRefusal> {
        let place_changes = self.place_changes;
        let directories_made = place_changes.make_directories_above(path, directory_marks)?;
        let made = match kind {
            EntryKind::File => place_changes.backing_directory.make_file(path).map(Some),
            EntryKind::Directory => place_changes.backing_directory.make_directory(path).map(|()| None),
        };
        let own_mark = directory_marks.get(path.components().count().saturating_sub(1));
        let refusal = match made {
            Ok(file) => {
                let undo = PlaceUndo::RemoveMade { path: path.to_owned(), directories_made };
                match kind {
                    EntryKind::File => return Ok((file, undo)),
                    EntryKind::Directory => match place_changes.mark_directory(path, own_mark) {
                        Ok(()) => return Ok((None, undo)),
                        Err(refusal) => {
                            if let Err(error) = self.undo(undo) {
                                tracing::warn!("cannot remove {} from the backing directory, which could not be marked: {error}", path.display());
                            }
                            return Err(refusal);
                        }
                    },
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && kind == EntryKind::Directory && place_changes.is_directory(path) => {
                // It was on disk, so the directories above it were: none was made.
                place_changes.mark_directory(path, own_mark)?;
                return Ok((None, PlaceUndo::UnmarkTaken { path: path.to_owned() }));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => PlaceRefusal::PlaceHeld { path: path.to_owned() },
            Err(error) => to_refusal(&error, path),
        };
        place_changes.remove_directories(&directories_made);
        Err(refusal)
    }

    /// Links the unnamed file `file` under `path`, with the directories above it that are
    /// not on disk made and marked, and syncs the directory. Nothing is replaced:
    /// `PlaceRefusal::PlaceHeld` when something is there. Undo: `RemoveMade`. What a
    /// landing of a download names its file by. The marks are read as
    /// [`make_entry`](Self::make_entry) reads them; every directory a name was made in is
    /// synced.
    pub(super) fn link_file(&self, file: &File, path: &Path, directory_marks: &DirectoryMarks) -> Result<PlaceUndo, PlaceRefusal> {
        let place_changes = self.place_changes;
        let directories_made = place_changes.make_directories_above(path, directory_marks)?;
        let linked = (|| {
            let unnamed = CString::new(konedrive_fs::proc_path(file).into_os_string().into_encoded_bytes())?;
            let named = CString::new(place_changes.backing_directory.resolve_path(path).into_os_string().into_encoded_bytes())?;
            // SAFETY: two paths this function owns.
            match unsafe { libc::linkat(libc::AT_FDCWD, unnamed.as_ptr(), libc::AT_FDCWD, named.as_ptr(), libc::AT_SYMLINK_FOLLOW) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        })();
        let refusal = match linked {
            Ok(()) => {
                let undo = PlaceUndo::RemoveMade { path: path.to_owned(), directories_made };
                let PlaceUndo::RemoveMade { directories_made, .. } = &undo else { unreachable!() };
                // The directory the name is in, and each one a directory was made in.
                let synced = directories_made.iter().chain([&path.to_owned()]).try_for_each(|made| place_changes.sync_directory(made.parent().unwrap_or(Path::new(""))));
                match synced {
                    Ok(()) => return Ok(undo),
                    Err(error) => {
                        let refusal = to_refusal(&error, path);
                        if let Err(error) = self.undo(undo) {
                            tracing::warn!("cannot remove {} from the backing directory, whose directory could not be synced: {error}", path.display());
                        }
                        return Err(refusal);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => PlaceRefusal::PlaceHeld { path: path.to_owned() },
            Err(error) => to_refusal(&error, path),
        };
        place_changes.remove_directories(&directories_made);
        Err(refusal)
    }

    /// Moves the entry at `from` to `to`, where the rows say nothing is: with
    /// `RENAME_NOREPLACE`, after making the directories above `to` that are not on disk,
    /// each marked. `PlaceRefusal::PlaceHeld` when the disk holds something there. A
    /// temporary name of an exchange is a `to` like any other. The marks are of the names
    /// of `to`, read as [`make_entry`](Self::make_entry) reads them; a moved directory is
    /// not marked again.
    pub(super) fn move_entry(&self, from: &Path, to: &Path, directory_marks: &DirectoryMarks) -> Result<PlaceUndo, PlaceRefusal> {
        let place_changes = self.place_changes;
        let directories_made = place_changes.make_directories_above(to, directory_marks)?;
        match place_changes.rename_without_replacing(from, to) {
            Ok(()) => Ok(PlaceUndo::MoveBack { from: from.to_owned(), to: to.to_owned(), directories_made }),
            Err(error) => {
                place_changes.remove_directories(&directories_made);
                Err(self.describe_move_refusal(&error, from, to))
            }
        }
    }

    /// Moves the entry at `from` over the entry at `to`, which the rows say is there and
    /// is the entry of `replaced`: that one is set aside first.
    ///
    /// When nothing is at `to` on disk — the replaced item has no copy there — the move
    /// is a plain one (`PlaceUndo::MoveBack`); the directory `to` is in must be on disk
    /// then: with no marks given none is made here, and a caller that knows the replaced
    /// item has nothing on disk calls [`move_entry`](Self::move_entry) with the marks. A
    /// directory at `to` that holds what no row says is not set aside: the place is held.
    pub(super) fn move_entry_over(&self, from: &Path, to: &Path, replaced: LocalItemId) -> Result<PlaceUndo, PlaceRefusal> {
        let place_changes = self.place_changes;
        let set_aside = PathBuf::from(make_aside_entry_name(replaced));
        let is_set_aside = match place_changes.inspect_entry_to_set_aside(to).map_err(|error| to_refusal(&error, to))? {
            EntryToSetAside::Nothing => false,
            EntryToSetAside::HeldDirectory => return Err(PlaceRefusal::PlaceHeld { path: to.to_owned() }),
            EntryToSetAside::Movable => {
                place_changes.move_aside(to, &set_aside).map_err(|error| to_refusal(&error, to))?;
                true
            }
        };
        match (place_changes.rename_without_replacing(from, to), is_set_aside) {
            (Ok(()), true) => Ok(PlaceUndo::MoveBackAndRestore { from: from.to_owned(), to: to.to_owned(), set_aside }),
            (Ok(()), false) => Ok(PlaceUndo::MoveBack { from: from.to_owned(), to: to.to_owned(), directories_made: Vec::new() }),
            (Err(error), is_set_aside) => {
                let refusal = self.describe_move_refusal(&error, from, to);
                if is_set_aside {
                    if let Err(error) = place_changes.rename_without_replacing(&set_aside, to) {
                        tracing::error!("cannot put {} back in the backing directory from {}, where it was set aside for a move that failed: {error}", to.display(), set_aside.display());
                    }
                }
                Err(refusal)
            }
        }
    }

    /// Sets the entry at `path`, the entry of `item`, aside under
    /// `folder::disk::make_aside_entry_name(item)` in the root: what a removal does first.
    /// `PlaceUndo::Nothing` when nothing is at `path`. A directory that is not empty on
    /// disk — it holds what no row says — is left where it is, with its mark taken off,
    /// and is `Nothing` too: a later directory made at that name takes it
    /// ([`make_entry`](Self::make_entry)), and nothing in it is ever removed by the
    /// daemon.
    pub(super) fn set_aside(&self, path: &Path, item: LocalItemId) -> Result<PlaceUndo, PlaceRefusal> {
        let place_changes = self.place_changes;
        match place_changes.inspect_entry_to_set_aside(path).map_err(|error| to_refusal(&error, path))? {
            EntryToSetAside::Nothing => Ok(PlaceUndo::Nothing),
            EntryToSetAside::HeldDirectory => {
                place_changes.backing_directory.clear_onedrive_id(path);
                Ok(PlaceUndo::Nothing)
            }
            EntryToSetAside::Movable => {
                let set_aside = PathBuf::from(make_aside_entry_name(item));
                place_changes.move_aside(path, &set_aside).map_err(|error| to_refusal(&error, path))?;
                Ok(PlaceUndo::Restore { set_aside, path: path.to_owned() })
            }
        }
    }

    /// Takes `undo` back. An error is the disk's: the caller asks for the mount's
    /// take-down (`MountTakeDown::ask`). Nothing is replaced by a move back either: what
    /// is in its way fails it.
    pub(super) fn undo(&self, undo: PlaceUndo) -> std::io::Result<()> {
        let place_changes = self.place_changes;
        match undo {
            PlaceUndo::Nothing => Ok(()),
            PlaceUndo::RemoveMade { path, directories_made } => {
                match place_changes.is_directory(&path) {
                    true => place_changes.backing_directory.remove_directory(&path)?,
                    false => place_changes.backing_directory.remove_file(&path)?,
                }
                place_changes.try_remove_directories(&directories_made)
            }
            PlaceUndo::UnmarkTaken { path } => {
                place_changes.backing_directory.clear_onedrive_id(&path);
                Ok(())
            }
            PlaceUndo::MoveBack { from, to, directories_made } => {
                place_changes.rename_without_replacing(&to, &from)?;
                place_changes.try_remove_directories(&directories_made)
            }
            PlaceUndo::MoveBackAndRestore { from, to, set_aside } => {
                place_changes.rename_without_replacing(&to, &from)?;
                place_changes.rename_without_replacing(&set_aside, &to)
            }
            PlaceUndo::Restore { set_aside, path } => place_changes.rename_without_replacing(&set_aside, &path),
        }
    }

    /// The change ended: its row is written and the disk has what the row says, or it was
    /// undone. The count is even again, and who sleeps on `ended` is woken, under
    /// `changes`. The same as dropping it; written out where the end must be seen.
    pub(super) fn end(self) {
        drop(self);
    }

    /// Why a move from `from` to `to` failed with `error`: the place is held, or the
    /// disk's errno with the path it is about.
    fn describe_move_refusal(&self, error: &io::Error, from: &Path, to: &Path) -> PlaceRefusal {
        match error.kind() {
            io::ErrorKind::AlreadyExists => PlaceRefusal::PlaceHeld { path: to.to_owned() },
            io::ErrorKind::NotFound if self.place_changes.backing_directory.read_metadata(from).is_none() => to_refusal(error, from),
            _ => to_refusal(error, to),
        }
    }
}

impl Drop for PlaceChange<'_> {
    /// The end of the change, also when its job returned early or panicked.
    fn drop(&mut self) {
        let held = crate::panic::lock(&self.place_changes.changes);
        self.place_changes.count.fetch_add(1, Ordering::SeqCst);
        self.place_changes.ended.notify_all();
        drop(held);
    }
}

#[cfg(test)]
mod tests;
