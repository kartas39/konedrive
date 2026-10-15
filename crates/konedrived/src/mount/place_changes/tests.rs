use std::collections::HashMap;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Barrier, Mutex, TryLockError};
use std::thread::{JoinHandle, ThreadId};

use konedrive_tree::LocalItemId;

use super::*;
use crate::folder::disk::{make_aside_entry_name, ASIDE_ENTRY_PREFIX};
use crate::sync_parts::step_outcome::{StepFailure, StepObstacle};

type Job = Box<dyn FnOnce() + Send>;

/// A thread that stands for the write-operation thread: the one thread a place change
/// may begin on. It runs the jobs it is given in their order.
struct ChangerThread {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    id: ThreadId,
}

impl ChangerThread {
    fn start() -> Self {
        let (jobs, queued) = mpsc::channel::<Job>();
        let (id_sender, id) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            id_sender.send(std::thread::current().id()).unwrap();
            for job in queued {
                job();
            }
        });
        ChangerThread { jobs: Some(jobs), thread: Some(thread), id: id.recv().unwrap() }
    }

    /// Has `job` run on the thread, and does not wait for it.
    fn post(&self, job: impl FnOnce() + Send + 'static) {
        self.jobs.as_ref().unwrap().send(Box::new(job)).unwrap();
    }

    /// Runs `job` on the thread and waits for what it gives.
    fn run<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> T {
        let (answer, answered) = mpsc::channel();
        self.post(move || {
            let _ = answer.send(job());
        });
        answered.recv().expect("the changer thread ended under a job")
    }
}

impl Drop for ChangerThread {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A place change the changer thread holds open until it is told to end it.
struct OpenChange {
    finish: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
}

impl OpenChange {
    fn end(self) {
        self.finish.send(()).unwrap();
        self.finished.recv().unwrap();
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    backing_directory: Arc<BackingDirectory>,
    place_changes: Arc<PlaceChanges>,
    changer: ChangerThread,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let backing_directory = Arc::new(BackingDirectory::open(directory.path()).unwrap());
    let changer = ChangerThread::start();
    let place_changes = Arc::new(PlaceChanges::new(Arc::clone(&backing_directory), changer.id));
    Fixture { directory, backing_directory, place_changes, changer }
}

impl Fixture {
    fn write(&self, rel: &str, content: &str) {
        let path = self.directory.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn make_directory(&self, rel: &str) {
        std::fs::create_dir_all(self.directory.path().join(rel)).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.directory.path().join(rel)).ok()
    }

    fn is_on_disk(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.directory.path().join(rel)).is_ok()
    }

    fn list_root(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.directory.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn read_mark(&self, rel: &str) -> Option<String> {
        self.backing_directory.read_onedrive_id(Path::new(rel))
    }

    fn count(&self) -> u64 {
        self.place_changes.count.load(Ordering::SeqCst)
    }

    /// One whole place change on the changer thread: begun, `change_disk`, ended.
    fn change<T: Send + 'static>(&self, change_disk: impl FnOnce(&PlaceChange<'_>) -> T + Send + 'static) -> T {
        let place_changes = Arc::clone(&self.place_changes);
        self.changer.run(move || {
            let change = place_changes.begin();
            let answer = change_disk(&change);
            change.end();
            answer
        })
    }

    /// A place change begun on the changer thread and held open; `ends_by_drop` lets it go
    /// as an early return would, without `end`.
    fn hold_change_open(&self, ends_by_drop: bool) -> OpenChange {
        let place_changes = Arc::clone(&self.place_changes);
        let (begun, is_begun) = mpsc::channel();
        let (finish, is_to_finish) = mpsc::channel();
        let (finished_sender, finished) = mpsc::channel();
        self.changer.post(move || {
            let change = place_changes.begin();
            begun.send(()).unwrap();
            is_to_finish.recv().unwrap();
            match ends_by_drop {
                true => drop(change),
                false => change.end(),
            }
            finished_sender.send(()).unwrap();
        });
        is_begun.recv().unwrap();
        OpenChange { finish, finished }
    }

    /// Waits, without sleeping, until a reader sleeps on the end of the change under way;
    /// `false` when `reader` ended instead.
    fn wait_for_sleeper<T>(&self, reader: &std::thread::ScopedJoinHandle<'_, T>) -> bool {
        while self.place_changes.sleeper_count.load(Ordering::SeqCst) == 0 {
            if reader.is_finished() {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    fn open_at(&self, rel: &'static str) -> Result<Option<String>, OpenAtRowError> {
        let opened = self.place_changes.open_at_row_path(|| Ok(Some((PathBuf::from(rel), ()))))?;
        Ok(opened.map(|(file, ())| read_whole(file)))
    }
}

fn read_whole(mut file: File) -> String {
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    content
}

fn no_marks() -> DirectoryMarks {
    Vec::new()
}

fn marks(ids: &[Option<&str>]) -> DirectoryMarks {
    ids.iter().map(|id| id.map(konedrive_tree::OneDriveItemId::new)).collect()
}

fn item(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn aside_path(number: i64) -> PathBuf {
    PathBuf::from(make_aside_entry_name(item(number)))
}

/// The rows of a test: the path each item has, read and written under one lock, as a row
/// and its path are read in one read transaction.
type Rows = Arc<Mutex<HashMap<&'static str, PathBuf>>>;

fn rows(places: &[(&'static str, &str)]) -> Rows {
    Arc::new(Mutex::new(places.iter().map(|(item, path)| (*item, PathBuf::from(path))).collect()))
}

fn find_row(rows: &Rows, item: &'static str) -> Result<Option<(PathBuf, &'static str)>, TreeError> {
    Ok(rows.lock().unwrap().get(item).map(|path| (path.clone(), item)))
}

/// A whole change as a write operation makes it: the disk first, the row second.
fn move_item(change: &PlaceChange<'_>, rows: &Rows, item: &'static str, to: &str) {
    let from = rows.lock().unwrap()[item].clone();
    change.move_entry(&from, Path::new(to), &no_marks()).unwrap();
    rows.lock().unwrap().insert(item, PathBuf::from(to));
}

// ---- The count and the readers ----

#[test]
fn the_count_is_odd_from_the_begin_of_a_change_to_its_end() {
    let f = fixture();
    assert_eq!(f.count(), 0);
    let open = f.hold_change_open(false);
    assert_eq!(f.count(), 1, "odd while the change is under way");
    open.end();
    assert_eq!(f.count(), 2, "even again");
    f.change(|_| ());
    assert_eq!(f.count(), 4);
}

#[test]
fn a_reader_that_began_during_a_change_sleeps_until_its_end_and_is_woken() {
    let f = fixture();
    f.write("a", "x");
    let open = f.hold_change_open(false);
    let reads_during_a_change = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            f.place_changes.open_at_row_path(|| {
                reads_during_a_change.fetch_add((f.count() % 2) as usize, Ordering::SeqCst);
                Ok(Some((PathBuf::from("a"), ())))
            })
        });
        assert!(f.wait_for_sleeper(&reader), "the reader did not wait for the end of the change");
        open.end();
        let (file, ()) = reader.join().unwrap().unwrap().unwrap();
        assert_eq!(read_whole(file), "x");
    });
    assert_eq!(reads_during_a_change.load(Ordering::SeqCst), 0, "no row is read while a change is under way");
    assert_eq!(f.place_changes.sleeper_count.load(Ordering::SeqCst), 0);
}

#[test]
fn a_reader_whose_open_was_crossed_by_a_change_tries_again_and_gets_the_file_of_the_row() {
    let f = fixture();
    f.write("a", "x");
    let rows = rows(&[("x", "a")]);
    let calls = AtomicU32::new(0);
    let opened = f.place_changes.open_at_row_path(|| {
        let found = find_row(&rows, "x");
        // The first row read is crossed: the file is moved before the reader opens.
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let rows = Arc::clone(&rows);
            f.change(move |change| move_item(change, &rows, "x", "b"));
        }
        found
    });
    let (file, item) = opened.unwrap().unwrap();
    assert_eq!((read_whole(file).as_str(), item), ("x", "x"));
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the row was read again after the change");
}

/// The race this part exists for: X from a to c, then Y from b to a, between the read of
/// X's row and the open of its path.
#[test]
fn two_renames_between_a_readers_row_and_its_open_never_give_it_the_other_items_file() {
    let f = fixture();
    f.write("a", "x");
    f.write("b", "y");
    let rows = rows(&[("x", "a"), ("y", "b")]);
    let calls = AtomicU32::new(0);
    let opened = f.place_changes.open_at_row_path(|| {
        let found = find_row(&rows, "x");
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            for (item, to) in [("x", "c"), ("y", "a")] {
                let rows = Arc::clone(&rows);
                f.change(move |change| move_item(change, &rows, item, to));
            }
            assert_eq!(f.read("a").as_deref(), Some("y"), "the path the reader holds is the other item's file now");
        }
        found
    });
    let (file, item) = opened.unwrap().unwrap();
    assert_eq!((read_whole(file).as_str(), item), ("x", "x"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Bounded: fixed counts, a barrier for the start, no sleep. The changer moves two entries
/// through each other's names; every reader must get the file of the row it read.
#[test]
fn readers_never_get_another_items_file_while_two_entries_are_renamed_back_and_forth() {
    const ROUNDS: usize = 200;
    const READERS: usize = 4;
    const READS: usize = 300;
    let f = fixture();
    f.write("a", "x");
    f.write("b", "y");
    let rows = rows(&[("x", "a"), ("y", "b")]);
    let start = Arc::new(Barrier::new(READERS + 1));
    let (place_changes, changer_rows, changer_start) = (Arc::clone(&f.place_changes), Arc::clone(&rows), Arc::clone(&start));
    let (changes_ended, are_changes_ended) = mpsc::channel();
    f.changer.post(move || {
        changer_start.wait();
        for _ in 0..ROUNDS {
            // Back to the start after six changes; each name is every item's in turn.
            for (item, to) in [("x", "c"), ("y", "a"), ("x", "b"), ("y", "c"), ("x", "a"), ("y", "b")] {
                let change = place_changes.begin();
                move_item(&change, &changer_rows, item, to);
                change.end();
            }
        }
        changes_ended.send(()).unwrap();
    });
    let most_tries = AtomicU32::new(0);
    std::thread::scope(|scope| {
        for reader in 0..READERS {
            let (f, rows, start, most_tries) = (&f, &rows, &start, &most_tries);
            scope.spawn(move || {
                let item = if reader % 2 == 0 { "x" } else { "y" };
                start.wait();
                for _ in 0..READS {
                    let mut tries = 0;
                    let opened = f.place_changes.open_at_row_path(|| {
                        tries += 1;
                        find_row(rows, item)
                    });
                    let (file, row) = opened.expect("a file of a row is on disk at every moment no change crosses").unwrap();
                    assert_eq!(read_whole(file), row, "a reader was given another item's file");
                    most_tries.fetch_max(tries, Ordering::SeqCst);
                }
            });
        }
    });
    are_changes_ended.recv().unwrap();
    assert!(most_tries.load(Ordering::SeqCst) <= CROSSED_TRIES + 1, "the retry is bounded");
    assert_eq!(f.count(), (ROUNDS * 6 * 2) as u64);
    assert_eq!((f.read("a").as_deref(), f.read("b").as_deref()), (Some("x"), Some("y")));
}

#[test]
fn after_three_crossed_tries_the_next_change_waits_for_the_readers_try() {
    let f = fixture();
    f.write("a", "x");
    let calls = AtomicU32::new(0);
    let was_lock_held = Mutex::new(Vec::new());
    // What happened in which order: the reader's last read, the next change's begin.
    let order = Arc::new(AtomicU64::new(0));
    let (read_at, begun_at) = (AtomicU64::new(0), Arc::new(AtomicU64::new(0)));
    let (began, has_begun) = mpsc::channel();
    let opened = f.place_changes.open_at_row_path(|| {
        let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
        was_lock_held.lock().unwrap().push(matches!(f.place_changes.changes.try_lock(), Err(TryLockError::WouldBlock)));
        if call <= CROSSED_TRIES {
            // A whole change crosses this try.
            f.change(|_| ());
        } else {
            // The next change is asked for while the reader is in its try; it is given
            // every chance to begin before the reader's read ends.
            let (place_changes, changer_order, begun_at, began) = (Arc::clone(&f.place_changes), Arc::clone(&order), Arc::clone(&begun_at), began.clone());
            f.changer.post(move || {
                let change = place_changes.begin();
                begun_at.store(changer_order.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                change.end();
                began.send(()).unwrap();
            });
            for _ in 0..1000 {
                std::thread::yield_now();
            }
            read_at.store(order.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        }
        Ok(Some((PathBuf::from("a"), ())))
    });
    let (file, ()) = opened.unwrap().unwrap();
    assert_eq!(read_whole(file), "x");
    assert_eq!(calls.load(Ordering::SeqCst), CROSSED_TRIES + 1, "the try after the crossed ones is the last");
    has_begun.recv().unwrap();
    assert_eq!(*was_lock_held.lock().unwrap(), [false, false, false, true], "`changes` is held for the last try alone");
    assert!(begun_at.load(Ordering::SeqCst) > read_at.load(Ordering::SeqCst), "the next change began before the reader's try was over");
}

#[test]
fn a_row_that_is_gone_answers_none_and_a_failure_of_the_store_is_told_as_such() {
    let f = fixture();
    let none = f.place_changes.open_at_row_path(|| Ok(None::<(PathBuf, ())>));
    assert!(matches!(none, Ok(None)));
    let failed = f.place_changes.open_at_row_path(|| Err::<Option<(PathBuf, ())>, _>(TreeError::Refused("no".into())));
    assert!(matches!(failed, Err(OpenAtRowError::Store(TreeError::Refused(_)))));
}

/// A reader never writes through what it opened.
#[test]
fn a_file_is_opened_to_read() {
    use std::io::Write;
    let f = fixture();
    f.write("a", "x");
    let (mut file, ()) = f.place_changes.open_at_row_path(|| Ok(Some((PathBuf::from("a"), ())))).unwrap().unwrap();
    assert!(file.write_all(b"z").is_err());
}

#[test]
fn a_stat_that_no_change_crossed_answers_the_facts_of_the_rows_file() {
    let f = fixture();
    f.write("docs/a", "four");
    let RowPathStat::Facts { facts, row } = f.place_changes.stat_at_row_path(|| Ok(Some((PathBuf::from("docs/a"), 7)))).unwrap() else { panic!("no facts") };
    assert_eq!((facts.kind, facts.size, row), (EntryKind::File, 4, 7));
    let RowPathStat::Facts { facts, row: () } = f.place_changes.stat_at_row_path(|| Ok(Some((PathBuf::from("docs"), ())))).unwrap() else { panic!("no facts") };
    assert_eq!(facts.kind, EntryKind::Directory);
    // Each of the other answers is told apart, with the row where one was read.
    assert_eq!(f.place_changes.stat_at_row_path(|| Ok(Some((PathBuf::from("gone"), 7)))).unwrap(), RowPathStat::NothingOnDisk { row: 7 });
    assert_eq!(f.place_changes.stat_at_row_path(|| Ok(None::<(PathBuf, i32)>)).unwrap(), RowPathStat::NoRow);
    assert!(f.place_changes.stat_at_row_path(|| Err::<Option<(PathBuf, ())>, _>(TreeError::Refused("no".into()))).is_err());
}

#[test]
fn a_stat_that_was_crossed_twice_answers_no_facts_and_does_not_sleep() {
    let f = fixture();
    f.write("a", "x");
    let calls = AtomicU32::new(0);
    let crossed = f.place_changes.stat_at_row_path(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        f.change(|_| ());
        Ok(Some((PathBuf::from("a"), 7)))
    });
    assert_eq!(crossed.unwrap(), RowPathStat::Crossed { row: Some(7) }, "no facts, and the row that was read");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "tried once more, and no further");
    // Crossed once: the second try answers.
    let calls = AtomicU32::new(0);
    let crossed_once = f.place_changes.stat_at_row_path(|| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            f.change(|_| ());
        }
        Ok(Some((PathBuf::from("a"), ())))
    });
    assert!(matches!(crossed_once.unwrap(), RowPathStat::Facts { facts, row: () } if facts.size == 1));
    // While a change is under way it answers at once, with the change still open.
    let open = f.hold_change_open(false);
    let calls = AtomicU32::new(0);
    let under_way = f.place_changes.stat_at_row_path(|| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some((PathBuf::from("a"), 7)))
    });
    assert_eq!(under_way.unwrap(), RowPathStat::Crossed { row: None }, "no row is read while a change is under way");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.count() % 2, 1, "the change is still under way: the stat did not wait for it");
    open.end();
}

#[test]
fn a_missing_file_at_a_moment_no_change_crossed_is_told_as_no_file() {
    let f = fixture();
    f.make_directory("docs");
    for missing in ["gone", "docs/gone", "no-directory/gone"] {
        match f.open_at(missing) {
            Err(OpenAtRowError::NoFile { path }) => assert_eq!(path, Path::new(missing)),
            other => panic!("{missing}: {other:?}"),
        }
    }
    f.write("file", "x");
    match f.open_at("file/below") {
        Err(OpenAtRowError::Disk { error, path }) => assert_eq!((error.raw_os_error(), path.as_path()), (Some(libc::ENOTDIR), Path::new("file/below")), "another error of the disk is not a missing file, and says where"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_errno_and_the_trouble_of_an_open_are_told_per_case() {
    let store = OpenAtRowError::Store(TreeError::Refused("no".into()));
    let no_file = OpenAtRowError::NoFile { path: PathBuf::from("a") };
    let disk = OpenAtRowError::Disk { error: std::io::Error::from_raw_os_error(libc::EACCES), path: PathBuf::from("docs/a") };
    let unnumbered = OpenAtRowError::Disk { error: std::io::Error::other("no number"), path: PathBuf::from("docs/a") };
    assert_eq!(decide_open_errno(&store), libc::EIO);
    assert_eq!(decide_open_errno(&no_file), libc::ENOENT);
    assert_eq!(decide_open_errno(&disk), libc::EACCES);
    assert_eq!(decide_open_errno(&unnumbered), libc::EIO);
    assert_eq!(decide_open_trouble(&no_file), StepTrouble::Obstacle(StepObstacle::BackingFileMissing));
    assert!(matches!(decide_open_trouble(&store), StepTrouble::Failure(StepFailure::Store { detail }) if detail.contains("no")));
    assert_eq!(decide_open_trouble(&disk), StepTrouble::Failure(StepFailure::Disk { errno: libc::EACCES, path: PathBuf::from("docs/a") }));
}

// ---- A change dropped, a panic, the lock ----

#[test]
fn a_change_dropped_without_its_end_still_ends_the_count_and_wakes_the_sleepers() {
    let f = fixture();
    f.write("a", "x");
    let open = f.hold_change_open(true);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| f.open_at("a"));
        assert!(f.wait_for_sleeper(&reader), "the reader did not wait for the end of the change");
        open.end();
        assert_eq!(reader.join().unwrap().unwrap().as_deref(), Some("x"));
    });
    assert_eq!(f.count(), 2);
}

#[test]
fn a_changer_that_panics_in_its_change_leaves_the_count_and_the_condition_usable() {
    let f = fixture();
    f.write("a", "x");
    let place_changes = Arc::clone(&f.place_changes);
    let (begun, is_begun) = mpsc::channel();
    let (go_on, may_go_on) = mpsc::channel::<()>();
    let (panicked, has_panicked) = mpsc::channel();
    // As the write-operation thread does: the panic of a job is caught, and the thread
    // goes on.
    f.changer.post(move || {
        let job = catch_unwind(AssertUnwindSafe(|| {
            let _change = place_changes.begin();
            begun.send(()).unwrap();
            may_go_on.recv().unwrap();
            panic!("a write operation panicked with its place change open");
        }));
        panicked.send(job.is_err()).unwrap();
    });
    is_begun.recv().unwrap();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| f.open_at("a"));
        assert!(f.wait_for_sleeper(&reader), "the reader did not wait for the end of the change");
        go_on.send(()).unwrap();
        assert_eq!(reader.join().unwrap().unwrap().as_deref(), Some("x"), "the sleeper was woken by the panic's unwinding");
    });
    assert!(has_panicked.recv().unwrap());
    assert_eq!(f.count(), 2, "even again");
    // The next change and the next reader go as if nothing happened.
    f.change(|change| change.move_entry(Path::new("a"), Path::new("b"), &no_marks()).unwrap());
    assert_eq!(f.open_at("b").unwrap().as_deref(), Some("x"));
    assert_eq!(f.count(), 4);
}

/// A reader holds `changes` across its read closure on its last try: a panic there
/// leaves the lock poisoned, and it is taken all the same.
#[test]
fn a_reader_that_panics_under_the_lock_leaves_it_usable() {
    let f = fixture();
    f.write("a", "x");
    let calls = AtomicU32::new(0);
    let reader = catch_unwind(AssertUnwindSafe(|| {
        f.place_changes.open_at_row_path(|| {
            if calls.fetch_add(1, Ordering::SeqCst) < CROSSED_TRIES {
                f.change(|_| ());
                return Ok(Some((PathBuf::from("a"), ())));
            }
            panic!("the read closure panicked under `changes`");
        })
    }));
    assert!(reader.is_err());
    assert!(f.place_changes.changes.is_poisoned(), "the panic was under the lock");
    f.change(|_| ());
    assert_eq!(f.open_at("a").unwrap().as_deref(), Some("x"));
    let open = f.hold_change_open(false);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| f.open_at("a"));
        assert!(f.wait_for_sleeper(&reader), "the reader did not wait for the end of the change");
        open.end();
        assert_eq!(reader.join().unwrap().unwrap().as_deref(), Some("x"));
    });
}

/// The order of locks, as the module's doc states it of `changes`: `begin` lets it go at
/// once, so that an open change holds no lock; a reader holds it for its last try alone
/// (`after_three_crossed_tries_the_next_change_waits_for_the_readers_try`), and `begin`
/// waits for it.
#[test]
fn a_change_holds_the_lock_for_its_begin_alone_and_a_begin_waits_for_who_holds_it() {
    let f = fixture();
    let open = f.hold_change_open(false);
    assert!(f.place_changes.changes.try_lock().is_ok(), "an open change holds no lock");
    open.end();
    // The test holds the lock as a reader in its last try does.
    let held = f.place_changes.changes.lock().unwrap();
    let order = Arc::new(AtomicU64::new(0));
    let begun_at = Arc::new(AtomicU64::new(0));
    let (asked, is_asked) = mpsc::channel();
    let (ended, has_ended) = mpsc::channel();
    let (place_changes, changer_order, changer_begun_at) = (Arc::clone(&f.place_changes), Arc::clone(&order), Arc::clone(&begun_at));
    f.changer.post(move || {
        asked.send(()).unwrap();
        let change = place_changes.begin();
        changer_begun_at.store(changer_order.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        change.end();
        ended.send(()).unwrap();
    });
    is_asked.recv().unwrap();
    for _ in 0..1000 {
        std::thread::yield_now();
    }
    assert_eq!(f.count() % 2, 0, "the count was raised under a held lock");
    let let_go_at = order.fetch_add(1, Ordering::SeqCst) + 1;
    drop(held);
    has_ended.recv().unwrap();
    assert!(begun_at.load(Ordering::SeqCst) > let_go_at, "the change began while the lock was held");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "write-operation thread")]
fn a_change_begun_off_the_write_operation_thread_is_a_fault() {
    let f = fixture();
    let _change = f.place_changes.begin();
}

#[cfg(debug_assertions)]
#[test]
fn an_open_by_a_rows_path_on_the_write_operation_thread_is_a_fault() {
    let f = fixture();
    f.write("a", "x");
    let place_changes = Arc::clone(&f.place_changes);
    let has_panicked = f.changer.run(move || catch_unwind(AssertUnwindSafe(|| place_changes.open_at_row_path(|| Ok(Some((PathBuf::from("a"), ())))).is_ok())).is_err());
    assert!(has_panicked, "with its own change open the thread would sleep on itself");
}

// ---- The disk ----

#[test]
fn a_move_to_a_place_held_on_disk_is_refused_and_replaces_nothing() {
    let f = fixture();
    f.write("a", "moved");
    f.write("b", "held");
    let refusal = f.change(|change| change.move_entry(Path::new("a"), Path::new("b"), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::PlaceHeld { path: PathBuf::from("b") }));
    assert_eq!((f.read("a").as_deref(), f.read("b").as_deref()), (Some("moved"), Some("held")));
    // Held below directories the move had to make: those are gone again.
    f.write("held-as-file", "x");
    let refusal = f.change(|change| change.move_entry(Path::new("a"), Path::new("held-as-file/a"), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::PlaceHeld { path: PathBuf::from("held-as-file") }), "a file where a directory above is to be");
    assert_eq!(f.read("held-as-file").as_deref(), Some("x"));
    let refusal = f.change(|change| change.move_entry(Path::new("gone"), Path::new("new/gone"), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::Disk { errno: libc::ENOENT, path: PathBuf::from("gone") }));
    assert!(!f.is_on_disk("new"), "the directory made for a move that was refused is removed again");
}

#[test]
fn a_move_makes_the_directories_above_marked_and_its_undo_removes_those_it_made() {
    let f = fixture();
    f.write("a", "x");
    f.make_directory("docs");
    let undo = f.change(|change| change.move_entry(Path::new("a"), Path::new("docs/new/deep/a"), &marks(&[Some("D-DOCS"), Some("D-NEW"), None]))).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBack { from: PathBuf::from("a"), to: PathBuf::from("docs/new/deep/a"), directories_made: vec![PathBuf::from("docs/new"), PathBuf::from("docs/new/deep")] });
    assert_eq!(f.read("docs/new/deep/a").as_deref(), Some("x"));
    assert!(!f.is_on_disk("a"));
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-DOCS"), "a directory above that was on disk carries its item's mark too");
    assert_eq!(f.read_mark("docs/new").as_deref(), Some("D-NEW"));
    assert_eq!(f.read_mark("docs/new/deep"), None, "a directory with no binding is made unmarked");
    f.change(move |change| change.undo(undo)).unwrap();
    assert_eq!(f.read("a").as_deref(), Some("x"));
    assert!(!f.is_on_disk("docs/new"), "what the move made is gone");
    assert!(f.is_on_disk("docs"), "what was there stays");
}

#[test]
fn a_file_or_a_directory_is_made_with_the_directories_above_and_removed_by_its_undo() {
    use std::io::Write;
    let f = fixture();
    let (file, undo) = f.change(|change| change.make_entry(Path::new("docs/a"), EntryKind::File, &marks(&[Some("D-DOCS")]))).unwrap();
    file.unwrap().write_all(b"x").unwrap();
    assert_eq!(f.read("docs/a").as_deref(), Some("x"));
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-DOCS"));
    assert_eq!(undo, PlaceUndo::RemoveMade { path: PathBuf::from("docs/a"), directories_made: vec![PathBuf::from("docs")] });
    f.change(move |change| change.undo(undo)).unwrap();
    assert!(!f.is_on_disk("docs"));
    // A directory, marked with its own item's id when the marks reach as far as itself.
    let (file, undo) = f.change(|change| change.make_entry(Path::new("docs/sub"), EntryKind::Directory, &marks(&[None, Some("D-SUB")]))).unwrap();
    assert!(file.is_none());
    assert_eq!((f.read_mark("docs"), f.read_mark("docs/sub").as_deref()), (None, Some("D-SUB")));
    f.change(move |change| change.undo(undo)).unwrap();
    assert!(!f.is_on_disk("docs"));
    // A file is refused where anything is, and a directory where a file is.
    f.write("held", "x");
    f.make_directory("held-directory");
    for (path, kind) in [("held", EntryKind::File), ("held-directory", EntryKind::File), ("held", EntryKind::Directory)] {
        let refusal = f.change(move |change| change.make_entry(Path::new(path), kind, &no_marks()).map(|_| ()));
        assert_eq!(refusal, Err(PlaceRefusal::PlaceHeld { path: PathBuf::from(path) }), "{path} {kind:?}");
    }
    assert_eq!(f.read("held").as_deref(), Some("x"));
}

#[test]
fn a_file_is_linked_under_its_name_with_nothing_replaced() {
    use std::io::Write;
    let f = fixture();
    let unnamed = || {
        let mut file = f.backing_directory.make_unnamed_file().unwrap();
        file.write_all(b"landed").unwrap();
        Arc::new(file)
    };
    let file = unnamed();
    let undo = f.change(move |change| change.link_file(&file, Path::new("docs/a"), &marks(&[Some("D-DOCS")]))).unwrap();
    assert_eq!(undo, PlaceUndo::RemoveMade { path: PathBuf::from("docs/a"), directories_made: vec![PathBuf::from("docs")] });
    assert_eq!(f.read("docs/a").as_deref(), Some("landed"));
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-DOCS"));
    // Nothing is replaced.
    f.write("held", "the user's");
    let file = unnamed();
    let refusal = f.change(move |change| change.link_file(&file, Path::new("held"), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::PlaceHeld { path: PathBuf::from("held") }));
    assert_eq!(f.read("held").as_deref(), Some("the user's"));
    let file = unnamed();
    let refusal = f.change(move |change| change.link_file(&file, Path::new("new/held/a"), &no_marks()).err());
    assert!(refusal.is_none(), "a free place below new directories");
    f.change(move |change| change.undo(undo)).unwrap();
    assert!(!f.is_on_disk("docs"), "the undo removes the link and the directory made for it");
}

#[test]
fn a_directory_made_where_one_is_on_disk_takes_it_and_marks_it_again() {
    let f = fixture();
    // What a removal left: a directory that held what no row says, its mark taken off.
    f.write("docs/by-hand", "the user's");
    let (file, undo) = f.change(|change| change.make_entry(Path::new("docs"), EntryKind::Directory, &marks(&[Some("D-NEW")]))).unwrap();
    assert!(file.is_none());
    assert_eq!(undo, PlaceUndo::UnmarkTaken { path: PathBuf::from("docs") });
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-NEW"));
    assert_eq!(f.read("docs/by-hand").as_deref(), Some("the user's"));
    f.change(move |change| change.undo(undo)).unwrap();
    assert_eq!(f.read_mark("docs"), None, "the mark is taken off again");
    assert_eq!(f.read("docs/by-hand").as_deref(), Some("the user's"), "the directory stays, with what it holds");
    // Taken with no mark written — none given, or the directory carries it already —
    // there is nothing to take back: the mark it has is not this change's.
    assert_eq!(f.change(|change| change.make_entry(Path::new("docs"), EntryKind::Directory, &no_marks()).map(|(_, undo)| undo)), Ok(PlaceUndo::Nothing));
    f.backing_directory.mark_with_onedrive_id(Path::new("docs"), "D-NEW").unwrap();
    let undo = f.change(|change| change.make_entry(Path::new("docs"), EntryKind::Directory, &marks(&[Some("D-NEW")])).map(|(_, undo)| undo)).unwrap();
    assert_eq!(undo, PlaceUndo::Nothing);
    f.change(move |change| change.undo(undo)).unwrap();
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-NEW"));
}

#[test]
fn an_entry_set_aside_is_put_back_by_its_undo_and_gone_after_its_discard() {
    let f = fixture();
    f.write("docs/a", "x");
    let undo = f.change(|change| change.set_aside(Path::new("docs/a"), item(7))).unwrap();
    assert_eq!(undo, PlaceUndo::Restore { set_aside: aside_path(7), path: PathBuf::from("docs/a") });
    assert!(!f.is_on_disk("docs/a"));
    assert_eq!(f.read(".konedrive-aside-7").as_deref(), Some("x"), "in the root, under its item's name");
    let put_back = undo.clone();
    f.change(move |change| change.undo(put_back)).unwrap();
    assert_eq!(f.read("docs/a").as_deref(), Some("x"));
    assert!(!f.is_on_disk(".konedrive-aside-7"));
    // Set aside again, and discarded after the change ended.
    let undo = f.change(|change| change.set_aside(Path::new("docs/a"), item(7))).unwrap();
    f.place_changes.discard(undo);
    assert_eq!(f.list_root(), ["docs"]);
    // An empty directory is set aside as a file is.
    f.make_directory("empty");
    let undo = f.change(|change| change.set_aside(Path::new("empty"), item(8))).unwrap();
    assert!(f.is_on_disk(".konedrive-aside-8") && !f.is_on_disk("empty"));
    f.place_changes.discard(undo);
    assert_eq!(f.list_root(), ["docs"]);
    // Nothing on disk: nothing to take back.
    assert_eq!(f.change(|change| change.set_aside(Path::new("gone"), item(9))), Ok(PlaceUndo::Nothing));
    f.place_changes.discard(PlaceUndo::Nothing);
}

#[test]
fn a_directory_that_holds_what_no_row_says_is_left_with_its_mark_off() {
    let f = fixture();
    f.write("docs/by-hand", "the user's");
    f.backing_directory.mark_with_onedrive_id(Path::new("docs"), "D-OLD").unwrap();
    let undo = f.change(|change| change.set_aside(Path::new("docs"), item(7))).unwrap();
    assert_eq!(undo, PlaceUndo::RestoreMark { path: PathBuf::from("docs"), mark: konedrive_tree::OneDriveItemId::new("D-OLD") });
    assert_eq!(f.read("docs/by-hand").as_deref(), Some("the user's"));
    assert_eq!(f.read_mark("docs"), None);
    assert_eq!(f.list_root(), ["docs"]);
    // The row could not be written: the mark is put back.
    let put_back = undo.clone();
    f.change(move |change| change.undo(put_back)).unwrap();
    assert_eq!(f.read_mark("docs").as_deref(), Some("D-OLD"));
    // The row was written: the discard removes nothing, and the mark stays off.
    let undo = f.change(|change| change.set_aside(Path::new("docs"), item(7))).unwrap();
    f.place_changes.discard(undo);
    assert_eq!((f.read_mark("docs"), f.read("docs/by-hand").as_deref()), (None, Some("the user's")));
    // One that carried no mark has nothing to put back.
    assert_eq!(f.change(|change| change.set_aside(Path::new("docs"), item(7))), Ok(PlaceUndo::Nothing));
}

#[test]
fn an_entry_left_set_aside_by_a_failed_discard_gives_way_to_the_next_of_its_item() {
    let f = fixture();
    f.write(".konedrive-aside-7", "left");
    f.write("a", "x");
    let undo = f.change(|change| change.set_aside(Path::new("a"), item(7))).unwrap();
    assert_eq!(f.read(".konedrive-aside-7").as_deref(), Some("x"));
    f.change(move |change| change.undo(undo)).unwrap();
    assert_eq!(f.read("a").as_deref(), Some("x"));
}

#[test]
fn a_move_over_an_entry_sets_it_aside_and_its_undo_puts_both_back() {
    let f = fixture();
    f.write("a", "moved");
    f.write("b", "displaced");
    let undo = f.change(|change| change.move_entry_over(Path::new("a"), Path::new("b"), item(7), &no_marks())).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBackAndRestore { from: PathBuf::from("a"), to: PathBuf::from("b"), set_aside: aside_path(7) });
    assert_eq!((f.read("b").as_deref(), f.read(".konedrive-aside-7").as_deref()), (Some("moved"), Some("displaced")));
    assert!(!f.is_on_disk("a"));
    let take_back = undo.clone();
    f.change(move |change| change.undo(take_back)).unwrap();
    assert_eq!((f.read("a").as_deref(), f.read("b").as_deref()), (Some("moved"), Some("displaced")));
    assert_eq!(f.list_root(), ["a", "b"]);
    // Done again and kept: the displaced entry goes at the discard.
    let undo = f.change(|change| change.move_entry_over(Path::new("a"), Path::new("b"), item(7), &no_marks())).unwrap();
    f.place_changes.discard(undo);
    assert_eq!(f.list_root(), ["b"]);
    assert_eq!(f.read("b").as_deref(), Some("moved"));
}

#[test]
fn a_move_over_what_is_not_on_disk_or_cannot_be_set_aside() {
    let f = fixture();
    f.write("a", "moved");
    // The displaced item has nothing on disk: a plain move.
    let undo = f.change(|change| change.move_entry_over(Path::new("a"), Path::new("b"), item(7), &no_marks())).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBack { from: PathBuf::from("a"), to: PathBuf::from("b"), directories_made: Vec::new() });
    assert_eq!(f.read("b").as_deref(), Some("moved"));
    // The replaced item has nothing on disk, nor has the directory it is in: that one is
    // made and marked, and the undo removes it again.
    let undo = f.change(|change| change.move_entry_over(Path::new("b"), Path::new("docs/new/b"), item(7), &marks(&[Some("D-DOCS"), Some("D-NEW")]))).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBack { from: PathBuf::from("b"), to: PathBuf::from("docs/new/b"), directories_made: vec![PathBuf::from("docs"), PathBuf::from("docs/new")] });
    assert_eq!((f.read("docs/new/b").as_deref(), f.read_mark("docs").as_deref(), f.read_mark("docs/new").as_deref()), (Some("moved"), Some("D-DOCS"), Some("D-NEW")));
    f.change(move |change| change.undo(undo)).unwrap();
    assert!(f.read("b").as_deref() == Some("moved") && !f.is_on_disk("docs"));
    // A directory that holds what no row says is not set aside: the place is held.
    f.make_directory("moved-directory");
    f.write("held/by-hand", "the user's");
    f.backing_directory.mark_with_onedrive_id(Path::new("held"), "D-HELD").unwrap();
    let refusal = f.change(|change| change.move_entry_over(Path::new("moved-directory"), Path::new("held"), item(8), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::PlaceHeld { path: PathBuf::from("held") }));
    assert_eq!(f.read_mark("held").as_deref(), Some("D-HELD"), "a refused move takes no mark off");
    // The moved entry is gone: the displaced one is put back, and the disk's errno told.
    let refusal = f.change(|change| change.move_entry_over(Path::new("gone"), Path::new("b"), item(7), &no_marks()));
    assert_eq!(refusal, Err(PlaceRefusal::Disk { errno: libc::ENOENT, path: PathBuf::from("gone") }));
    assert_eq!(f.read("b").as_deref(), Some("moved"));
    assert!(!f.is_on_disk(".konedrive-aside-7"));
}

#[test]
fn the_entries_still_set_aside_after_the_check_are_removed_before_the_mount_goes_up() {
    let f = fixture();
    f.write(".konedrive-aside-7", "a file");
    f.make_directory(".konedrive-aside-8");
    f.write(".konedrive-aside-9/below", "a directory that was not empty");
    f.write("docs/.konedrive-aside-7", "not in the root: a file of the folder's");
    f.write("kept", "x");
    assert_eq!(f.place_changes.remove_aside_entries().unwrap(), 3);
    assert_eq!(f.list_root(), ["docs", "kept"]);
    assert!(f.is_on_disk("docs/.konedrive-aside-7"));
    assert_eq!(f.place_changes.remove_aside_entries().unwrap(), 0);
    assert_eq!(f.count(), 0, "no place change: no row says these entries");
}

#[test]
fn a_temporary_name_is_moved_to_like_any_place_and_is_not_cleaned() {
    let f = fixture();
    f.write("docs/a", "x");
    let temporary_name = PathBuf::from("docs").join(konedrive_tree::make_temporary_name(item(7), 1));
    let to = temporary_name.clone();
    let undo = f.change(move |change| change.move_entry(Path::new("docs/a"), &to, &no_marks())).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBack { from: PathBuf::from("docs/a"), to: temporary_name.clone(), directories_made: Vec::new() });
    // In the root too: it is a row's place, and stays.
    f.write("b", "y");
    let in_root = PathBuf::from(konedrive_tree::make_temporary_name(item(8), 1));
    let to = in_root.clone();
    f.change(move |change| change.move_entry(Path::new("b"), &to, &no_marks())).unwrap();
    assert!(!in_root.to_str().unwrap().starts_with(ASIDE_ENTRY_PREFIX));
    assert_eq!(f.place_changes.remove_aside_entries().unwrap(), 0);
    assert_eq!(f.read(in_root.to_str().unwrap()).as_deref(), Some("y"));
    assert_eq!(f.read(temporary_name.to_str().unwrap()).as_deref(), Some("x"));
}

#[test]
fn an_undo_that_fails_is_answered_as_such_and_asks_nothing_by_itself() {
    let f = fixture();
    f.write("a", "x");
    let undo = f.change(|change| change.move_entry(Path::new("a"), Path::new("b"), &no_marks())).unwrap();
    // The old place is taken meanwhile: the move back replaces nothing, and fails.
    f.write("a", "in the way");
    let failed = f.change(move |change| change.undo(undo).map_err(|error| error.raw_os_error()));
    assert_eq!(failed, Err(Some(libc::EEXIST)));
    assert_eq!((f.read("a").as_deref(), f.read("b").as_deref()), (Some("in the way"), Some("x")), "nothing was replaced");
    assert_eq!(f.count(), 4, "the change ended as any other");
    assert_eq!(f.change(|change| change.undo(PlaceUndo::Nothing).is_ok()), true);
    // What a restore finds in its way fails it likewise.
    f.write("c", "y");
    let undo = f.change(|change| change.set_aside(Path::new("c"), item(7))).unwrap();
    f.write("c", "in the way");
    let failed = f.change(move |change| change.undo(undo).map_err(|error| error.raw_os_error()));
    assert_eq!(failed, Err(Some(libc::EEXIST)));
    assert_eq!(f.read(".konedrive-aside-7").as_deref(), Some("y"));
}

/// The name the caller's record stores, and the check at start puts an entry back from:
/// the aside name of the item, in the ROOT of the backing directory, however deep the
/// entry was.
#[test]
fn an_entry_several_directories_down_is_set_aside_in_the_root_under_its_items_name() {
    let f = fixture();
    f.write("a/b/c/file", "x");
    f.write("a/b/c/displaced", "y");
    f.write("a/moved", "z");
    let undo = f.change(|change| change.set_aside(Path::new("a/b/c/file"), item(41))).unwrap();
    assert_eq!(undo, PlaceUndo::Restore { set_aside: PathBuf::from(".konedrive-aside-41"), path: PathBuf::from("a/b/c/file") });
    let undo = f.change(|change| change.move_entry_over(Path::new("a/moved"), Path::new("a/b/c/displaced"), item(42), &no_marks())).unwrap();
    assert_eq!(undo, PlaceUndo::MoveBackAndRestore { from: PathBuf::from("a/moved"), to: PathBuf::from("a/b/c/displaced"), set_aside: PathBuf::from(".konedrive-aside-42") });
    assert_eq!(f.list_root(), [".konedrive-aside-41", ".konedrive-aside-42", "a"]);
    assert_eq!((f.read(".konedrive-aside-41").as_deref(), f.read(".konedrive-aside-42").as_deref()), (Some("x"), Some("y")));
    assert_eq!(std::fs::read_dir(f.directory.path().join("a/b/c")).unwrap().count(), 1, "nothing is set aside beside the entry");
    assert_eq!(aside_path(41), PathBuf::from(make_aside_entry_name(item(41))));
}
