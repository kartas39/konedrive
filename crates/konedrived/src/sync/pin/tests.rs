use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::sync::Semaphore;

use super::*;
use crate::sync::SyncSnapshot;

/// The pool the tests' pins download in: four slots, never more.
const PIN_SLOTS: usize = 4;

/// Downloads nothing: each fill counts itself, waits for the test to let
/// it through, and answers `answer`. One whose future is dropped while it
/// waits is counted in `dropped`.
struct Held {
    answer: Filled,
    started: AtomicUsize,
    /// The files started, in order.
    order: Mutex<Vec<PathBuf>>,
    dropped: Arc<AtomicUsize>,
    gate: Semaphore,
}

impl Held {
    fn new(answer: Filled) -> Arc<Self> {
        Arc::new(Self {
            answer,
            started: AtomicUsize::new(0),
            order: Mutex::default(),
            dropped: Arc::default(),
            gate: Semaphore::new(0),
        })
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

struct CountsDrop(Arc<AtomicUsize>);

impl Drop for CountsDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl PinFill for Held {
    async fn fill_pinned(&self, path: &Path) -> Filled {
        self.order.lock().unwrap().push(path.to_path_buf());
        self.started.fetch_add(1, Ordering::SeqCst);
        let dropped = CountsDrop(Arc::clone(&self.dropped));
        self.gate.acquire().await.expect("never closed").forget();
        std::mem::forget(dropped);
        self.answer
    }
}

fn pins_for(held: &Arc<Held>) -> Arc<Pins> {
    pins_in(held, TransferPool::starting_at(PIN_SLOTS, PIN_SLOTS))
}

fn pins_in(held: &Arc<Held>, pool: Arc<TransferPool>) -> Arc<Pins> {
    let state = SyncStateHandle::new(SyncSnapshot { root_path: "/r".into(), ..SyncSnapshot::default() });
    let filler: Weak<dyn PinFill> = Arc::downgrade(held) as Weak<Held>;
    Pins::new(state, filler, pool)
}

fn files(n: usize) -> Vec<Wanted> {
    (0..n).map(|i| (PathBuf::from(format!("/r/{i}.bin")), 1024)).collect()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

/// A full disk drops everything still waiting, rather than failing it
/// file by file; the downloads under way end on their own, and the next
/// cycle is told to sweep. A file is queued once, however often given.
#[tokio::test]
async fn a_full_disk_drops_what_waits_and_asks_for_a_sweep() {
    let held = Held::new(Filled::NoSpace);
    let pins = pins_for(&held);
    assert_eq!(pins.add(files(10)), 10);
    assert_eq!(pins.add(files(3)), 0, "pending or under way already");
    until("four downloads under way", || held.started() == PIN_SLOTS).await;

    held.gate.add_permits(1);
    until("the waiting files dropped", || pins.queued().len() == PIN_SLOTS - 1).await;
    held.gate.add_permits(PIN_SLOTS);
    until("the queue empty", || pins.queued().is_empty()).await;

    assert_eq!(held.started(), PIN_SLOTS, "nothing that waited was tried");
    assert!(pins.take_resweep());
    assert!(!pins.take_resweep(), "asking settles it");
}

/// A Forget cancels the downloads under way and drops the rest.
#[tokio::test]
async fn a_forget_cancels_the_downloads_under_way() {
    let held = Held::new(Filled::Done);
    let pins = pins_for(&held);
    pins.add(files(6));
    until("four downloads under way", || held.started() == PIN_SLOTS).await;

    pins.clear();

    until("every download under way cancelled", || held.dropped.load(Ordering::SeqCst) == PIN_SLOTS).await;
    assert!(pins.queued().is_empty());
    assert_eq!(held.started(), PIN_SLOTS, "what waited was dropped, not started");
}

/// Folder by folder, alphabetically: a folder's files by name first, then its
/// subfolders by name, each the same way.
#[test]
fn pinned_downloads_go_folder_by_folder_in_alphabetical_order() {
    let mut files: Vec<Wanted> = ["/r/b/z.txt", "/r/B.txt", "/r/a/c/1.txt", "/r/a.txt", "/r/a/2.txt", "/r/a/b/3.txt", "/r/C.txt"]
        .into_iter()
        .map(|p| (PathBuf::from(p), 0))
        .collect();
    in_folder_order(&mut files);
    let order: Vec<&str> = files.iter().map(|(p, _)| p.to_str().unwrap()).collect();
    assert_eq!(order, ["/r/a.txt", "/r/B.txt", "/r/C.txt", "/r/a/2.txt", "/r/a/b/3.txt", "/r/a/c/1.txt", "/r/b/z.txt"]);
}

/// Digits compare as numbers, as Dolphin sorts: `file2` before `file10`.
#[test]
fn numbers_in_names_sort_by_value() {
    let mut files: Vec<Wanted> = ["/r/file10.txt", "/r/file2.txt", "/r/File1.txt", "/r/file02b.txt", "/r/file.txt"]
        .into_iter()
        .map(|p| (PathBuf::from(p), 0))
        .collect();
    in_folder_order(&mut files);
    let order: Vec<&str> = files.iter().map(|(p, _)| p.to_str().unwrap()).collect();
    assert_eq!(order, ["/r/file.txt", "/r/File1.txt", "/r/file2.txt", "/r/file02b.txt", "/r/file10.txt"]);
}

/// A large file waiting for the large-file limit lets the small files queued behind it
/// go; the queue's order holds otherwise.
#[tokio::test]
async fn a_large_file_waiting_for_the_limit_does_not_hold_up_the_small_ones() {
    let held = Held::new(Filled::Done);
    let pool = TransferPool::starting_at(PIN_SLOTS, PIN_SLOTS);
    pool.set_limits(PIN_SLOTS, 1);
    let pins = pins_in(&held, pool);
    let large = crate::pool::LARGE_FROM;
    pins.add(vec![
        (PathBuf::from("/r/big-1.bin"), large),
        (PathBuf::from("/r/big-2.bin"), large),
        (PathBuf::from("/r/small-1.bin"), 10),
        (PathBuf::from("/r/small-2.bin"), 10),
        (PathBuf::from("/r/small-3.bin"), 10),
    ]);
    until("four downloads under way", || held.started() == PIN_SLOTS).await;
    let started: HashSet<PathBuf> = held.order.lock().unwrap().iter().cloned().collect();
    let expected: HashSet<PathBuf> =
        ["/r/big-1.bin", "/r/small-1.bin", "/r/small-2.bin", "/r/small-3.bin"].into_iter().map(PathBuf::from).collect();
    assert_eq!(started, expected, "the second large file waits; the small ones behind it do not");

    held.gate.add_permits(PIN_SLOTS + 1);
    until("the queue empty", || pins.queued().is_empty()).await;
    assert_eq!(held.order.lock().unwrap().last().unwrap(), Path::new("/r/big-2.bin"));
}
