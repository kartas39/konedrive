use std::sync::mpsc;

use konedrive_tree::{ItemPlace, LocalItemId};

use super::*;

fn appeared(number: i64) -> ItemAnnouncement {
    ItemAnnouncement::Appeared { item: LocalItemId::new(number), place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("item-{number}").into() } }
}

fn bridge() -> Arc<ItemInotifyBridge> {
    Arc::new(ItemInotifyBridge::new(PathBuf::from("/folder")))
}

/// A thread that takes what is queued as the bridge's own does, and passes on what it
/// took; kept by the bridge as its thread.
fn start_taking(bridge: &Arc<ItemInotifyBridge>) -> mpsc::Receiver<Vec<ItemAnnouncement>> {
    let (taken_sender, taken) = mpsc::channel();
    let taking = Arc::clone(bridge);
    let thread = std::thread::spawn(move || {
        while let Some(announcements) = taking.wait_for_announcements() {
            taken_sender.send(announcements).unwrap();
        }
    });
    *bridge.thread.lock().unwrap() = Some(thread);
    taken
}

#[test]
fn the_announcement_being_performed_is_what_the_bridges_own_lookup_is_answered_from() {
    let bridge = bridge();
    assert_eq!(bridge.find_in_progress(), None);
    bridge.mark_in_progress(Some(appeared(5)));
    assert_eq!(bridge.find_in_progress(), Some(appeared(5)));
    bridge.mark_in_progress(Some(appeared(6)));
    assert_eq!(bridge.find_in_progress(), Some(appeared(6)));
    bridge.mark_in_progress(None);
    assert_eq!(bridge.find_in_progress(), None);
    // The bridge's requests are told by the kernel's id of its thread.
    assert!(!bridge.is_own_thread(0) && !bridge.is_own_thread(77), "no thread yet: no request is its own");
    bridge.thread_id.set(77).unwrap();
    assert!(bridge.is_own_thread(77) && !bridge.is_own_thread(78));
}

#[test]
fn announcements_are_performed_in_the_order_they_were_queued() {
    let bridge = bridge();
    // Queued before the thread runs, as while the mount is built and not served.
    bridge.announce(vec![appeared(1), appeared(2)]);
    bridge.announce(Vec::new());
    bridge.announce(vec![appeared(3)]);
    assert_eq!(bridge.wait_for_announcements(), Some(vec![appeared(1), appeared(2)]), "the announcements of one change stay together");
    assert_eq!(bridge.wait_for_announcements(), Some(vec![appeared(3)]), "a change with nothing to announce queues nothing");
    // And while the thread waits for them.
    let taken = start_taking(&bridge);
    for number in 4..20 {
        bridge.announce(vec![appeared(number)]);
    }
    for number in 4..20 {
        assert_eq!(taken.recv().unwrap(), [appeared(number)]);
    }
    bridge.stop();
}

#[test]
fn a_stop_drops_what_is_queued_and_ends_the_thread() {
    let bridge = bridge();
    let taken = start_taking(&bridge);
    bridge.announce(vec![appeared(1)]);
    assert_eq!(taken.recv().unwrap(), [appeared(1)]);
    // The thread waits for the next; the stop ends its wait and waits for its end.
    bridge.stop();
    assert!(taken.recv().is_err(), "the thread ended");
    assert!(bridge.thread.lock().unwrap().is_none(), "and was waited for");
    // What comes later is dropped, and what was queued is gone.
    bridge.announce(vec![appeared(2)]);
    assert!(bridge.queued.lock().unwrap().is_empty());
    assert_eq!(bridge.wait_for_announcements(), None);
    bridge.stop();
    // Queued and never taken: dropped by the stop.
    let bridge = self::bridge();
    bridge.announce(vec![appeared(3)]);
    bridge.stop();
    assert!(bridge.queued.lock().unwrap().is_empty());
    assert_eq!(bridge.wait_for_announcements(), None);
}

// The thread, with the mount replaced by a directory the test looks into: the names are
// really made and removed there, where the mount would answer them from the rows.

use std::fs;
use std::time::{Duration, Instant, UNIX_EPOCH};

use konedrive_tree::{ItemKind, ItemPlace as Place};

use crate::mount::item_fixture::ItemFixture;

/// A bridge over a plain directory, started: the paths it announces at are read from the
/// fixture's rows.
fn start_bridge(fixture: &ItemFixture) -> (Arc<ItemInotifyBridge>, tempfile::TempDir) {
    let watched = tempfile::tempdir().unwrap();
    let bridge = Arc::new(ItemInotifyBridge::new(watched.path().to_owned()));
    bridge.start(Arc::clone(&fixture.read_connections), None).unwrap();
    (bridge, watched)
}

/// Waits until `is_so`, for no longer than ten seconds.
fn wait_until(what: &str, is_so: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !is_so() {
        assert!(Instant::now() < deadline, "the bridge's thread did not get there: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn place_in(directory: LocalItemId, name: &str) -> Place {
    Place { parent_local_item_id: directory, name: name.into() }
}

#[test]
fn an_appearing_is_a_name_created_and_a_vanishing_a_name_removed_by_path() {
    let fixture = ItemFixture::new();
    let docs = fixture.show_onedrive_directory("docs").local_item_id;
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello").local_item_id;
    let sub = fixture.show_onedrive_directory("docs/sub").local_item_id;
    let top = fixture.show_onedrive_file("top.txt", b"hello").local_item_id;
    let (bridge, watched) = start_bridge(&fixture);
    let at = |path: &str| watched.path().join(path);
    fs::create_dir(at("docs")).unwrap();
    // An appearing: the kind is the row's, the path the directory's by the rows.
    bridge.announce(vec![ItemAnnouncement::Appeared { item: file, place: place_in(docs, "a.txt") }, ItemAnnouncement::Appeared { item: sub, place: place_in(docs, "sub") }]);
    bridge.announce(vec![ItemAnnouncement::Appeared { item: top, place: place_in(LocalItemId::ROOT, "top.txt") }]);
    wait_until("the three names appear", || at("top.txt").exists());
    assert!(at("docs/a.txt").is_file() && at("docs/sub").is_dir() && at("top.txt").is_file(), "performed in the order they were queued, each of its kind");
    // A vanishing: the former place and the kind travel in the announcement; the rows
    // need not have the item any more.
    fs::write(at("docs/old.txt"), b"").unwrap();
    fs::create_dir(at("docs/old")).unwrap();
    let gone = LocalItemId::new(987_654);
    bridge.announce(vec![
        ItemAnnouncement::Vanished { item: gone, former_place: place_in(docs, "old.txt"), kind: ItemKind::File },
        ItemAnnouncement::Vanished { item: gone, former_place: place_in(docs, "old"), kind: ItemKind::Directory },
        // A move: the vanishing, then the appearing.
        ItemAnnouncement::Vanished { item: file, former_place: place_in(docs, "a.txt"), kind: ItemKind::File },
        ItemAnnouncement::Appeared { item: file, place: place_in(docs, "b.txt") },
    ]);
    wait_until("the last name appears", || at("docs/b.txt").exists());
    assert!(!at("docs/old.txt").exists() && !at("docs/old").exists() && !at("docs/a.txt").exists());
    bridge.stop();
}

#[test]
fn the_thread_is_started_once_told_by_its_id_and_ended_by_the_stop() {
    let fixture = ItemFixture::new();
    let top = fixture.show_onedrive_file("top.txt", b"hello").local_item_id;
    let (bridge, watched) = start_bridge(&fixture);
    // SAFETY: a system call that takes nothing and cannot fail.
    let own_id = unsafe { libc::gettid() } as u32;
    let thread_id = *bridge.thread_id.get().expect("known when the start returns");
    assert!(thread_id != own_id && bridge.is_own_thread(thread_id) && !bridge.is_own_thread(own_id), "the kernel's id of the bridge's thread, not of the starter's");
    assert!(bridge.thread.lock().unwrap().is_some(), "kept for the stop to wait for");
    // A second start starts nothing.
    bridge.start(Arc::clone(&fixture.read_connections), None).unwrap();
    assert!(bridge.is_own_thread(thread_id));
    bridge.announce(vec![ItemAnnouncement::Appeared { item: top, place: place_in(LocalItemId::ROOT, "top.txt") }]);
    wait_until("the name appears", || watched.path().join("top.txt").exists());
    // The stop ends the thread and waits for it: nothing is made by path after it.
    bridge.stop();
    assert!(bridge.thread.lock().unwrap().is_none(), "the thread was waited for");
    fs::remove_file(watched.path().join("top.txt")).unwrap();
    bridge.announce(vec![ItemAnnouncement::Appeared { item: top, place: place_in(LocalItemId::ROOT, "top.txt") }]);
    assert!(bridge.queued.lock().unwrap().is_empty() && bridge.find_in_progress().is_none());
    assert!(!watched.path().join("top.txt").exists());
    // And a start after the stop starts no thread.
    bridge.start(Arc::clone(&fixture.read_connections), None).unwrap();
    assert!(bridge.thread.lock().unwrap().is_none());
    // A bridge that was never started is stopped all the same; a start then is nothing.
    let never_started = Arc::new(ItemInotifyBridge::new(watched.path().to_owned()));
    never_started.stop();
    never_started.start(Arc::clone(&fixture.read_connections), None).unwrap();
    assert!(never_started.thread.lock().unwrap().is_none() && never_started.thread_id.get().is_none());
}

/// An inotify watch of one directory: what the kernel told a watcher of it, in order.
struct DirectoryWatch(std::os::fd::OwnedFd);

impl DirectoryWatch {
    fn of(directory: &std::path::Path) -> Self {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(directory.as_os_str().as_bytes()).unwrap();
        // SAFETY: plain system calls; the descriptor is owned from here on.
        unsafe {
            let descriptor = libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC);
            assert!(descriptor >= 0 && libc::inotify_add_watch(descriptor, path.as_ptr(), libc::IN_CREATE | libc::IN_DELETE | libc::IN_ATTRIB) >= 0, "the directory is watched");
            DirectoryWatch(std::os::fd::OwnedFd::from_raw_fd(descriptor))
        }
    }

    /// The events told so far and not taken yet: what happened, and to which name.
    fn take_events(&self) -> Vec<(&'static str, String)> {
        use std::os::fd::AsRawFd;
        let (mut events, mut buffer) = (Vec::new(), [0u8; 4096]);
        loop {
            // SAFETY: a read into a buffer this function owns.
            let read = unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                return events;
            }
            let (mut at, header) = (0, std::mem::size_of::<libc::inotify_event>());
            while at + header <= read as usize {
                // SAFETY: the kernel wrote a whole event at `at`; read unaligned.
                let event: libc::inotify_event = unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(at).cast()) };
                let name = &buffer[at + header..at + header + event.len as usize];
                let name = String::from_utf8_lossy(name.split(|byte| *byte == 0).next().unwrap_or_default()).into_owned();
                let what = [(libc::IN_CREATE, "made"), (libc::IN_DELETE, "removed"), (libc::IN_ATTRIB, "touched")].into_iter().find(|(mask, _)| event.mask & mask != 0).map_or("other", |(_, what)| what);
                events.push((what, name));
                at += header + event.len as usize;
            }
        }
    }
}

#[test]
fn an_announcement_is_performed_once() {
    let fixture = ItemFixture::new();
    let first = fixture.show_onedrive_file("first.txt", b"hello").local_item_id;
    let second = fixture.show_onedrive_file("second.txt", b"hello").local_item_id;
    let (bridge, watched) = start_bridge(&fixture);
    let watch = DirectoryWatch::of(watched.path());
    let in_root = |name: &str| place_in(LocalItemId::ROOT, name);
    // One change whose announcements undo each other: performed a second time, it would
    // be told a second time. Then the one to wait for.
    bridge.announce(vec![
        ItemAnnouncement::Appeared { item: first, place: in_root("first.txt") },
        ItemAnnouncement::ContentChanged { item: first, place: in_root("first.txt") },
        ItemAnnouncement::Vanished { item: first, former_place: in_root("first.txt"), kind: ItemKind::File },
    ]);
    bridge.announce(vec![ItemAnnouncement::Appeared { item: second, place: in_root("second.txt") }]);
    wait_until("the last name appears", || watched.path().join("second.txt").exists());
    wait_until("nothing is in progress", || bridge.find_in_progress().is_none());
    bridge.stop();
    // What a watcher of the directory was told: each change once, in the order queued.
    // (The kernel folds two equal events in a row into one: a touch made twice at once
    // would not be seen here; a change performed again is.)
    let told = watch.take_events();
    let expected = [("made", "first.txt"), ("touched", "first.txt"), ("removed", "first.txt"), ("made", "second.txt")];
    assert_eq!(told, expected.map(|(what, name)| (what, name.to_owned())));
    assert!(bridge.queued.lock().unwrap().is_empty());
}

#[test]
fn a_changed_content_is_a_touch_of_the_file_by_path() {
    let fixture = ItemFixture::new();
    let docs = fixture.show_onedrive_directory("docs").local_item_id;
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello").local_item_id;
    let (bridge, watched) = start_bridge(&fixture);
    let path = watched.path().join("docs/a.txt");
    fs::create_dir(watched.path().join("docs")).unwrap();
    fs::write(&path, b"").unwrap();
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(&path).unwrap().set_modified(long_ago).unwrap();
    bridge.announce(vec![ItemAnnouncement::ContentChanged { item: file, place: place_in(docs, "a.txt") }]);
    wait_until("the file is touched", || fs::metadata(&path).unwrap().modified().unwrap() > long_ago);
    // A directory has no content: nothing is touched for one.
    let directory = watched.path().join("docs");
    fs::File::open(&directory).unwrap().set_modified(long_ago).unwrap();
    bridge.announce(vec![ItemAnnouncement::ContentChanged { item: docs, place: place_in(LocalItemId::ROOT, "docs") }]);
    fs::File::options().write(true).open(&path).unwrap().set_modified(long_ago).unwrap();
    bridge.announce(vec![ItemAnnouncement::ContentChanged { item: file, place: place_in(docs, "a.txt") }]);
    wait_until("the file is touched again", || fs::metadata(&path).unwrap().modified().unwrap() > long_ago);
    assert_eq!(fs::metadata(&directory).unwrap().modified().unwrap(), long_ago);
    bridge.stop();
}

/// The stand-in of the kernel's cached pages: which file's pages were dropped, and the
/// time the file at `path` had by then.
struct RecordedPageCache {
    path: PathBuf,
    dropped: std::sync::Mutex<Vec<(LocalItemId, std::time::SystemTime)>>,
}

impl KernelPageCache for RecordedPageCache {
    fn invalidate(&self, node: LocalItemId) -> std::io::Result<()> {
        self.dropped.lock().unwrap().push((node, fs::metadata(&self.path)?.modified()?));
        Ok(())
    }
}

/// The review's S5: after a step took OneDrive's content, no read through the mount may
/// be answered pages of the old one.
#[test]
fn the_kernels_pages_of_a_changed_content_are_dropped_before_the_file_is_touched() {
    let fixture = ItemFixture::new();
    let docs = fixture.show_onedrive_directory("docs").local_item_id;
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello").local_item_id;
    let watched = tempfile::tempdir().unwrap();
    let path = watched.path().join("docs/a.txt");
    fs::create_dir(watched.path().join("docs")).unwrap();
    fs::write(&path, b"").unwrap();
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(&path).unwrap().set_modified(long_ago).unwrap();
    let page_cache = Arc::new(RecordedPageCache { path: path.clone(), dropped: Default::default() });
    let bridge = Arc::new(ItemInotifyBridge::new(watched.path().to_owned()));
    bridge.start(Arc::clone(&fixture.read_connections), Some(Arc::clone(&page_cache) as Arc<dyn KernelPageCache>)).unwrap();
    bridge.announce(vec![
        // A name that appears has no pages, and a directory no content.
        ItemAnnouncement::Appeared { item: file, place: place_in(docs, "b.txt") },
        ItemAnnouncement::ContentChanged { item: docs, place: place_in(LocalItemId::ROOT, "docs") },
        ItemAnnouncement::ContentChanged { item: file, place: place_in(docs, "a.txt") },
    ]);
    wait_until("the file is touched", || fs::metadata(&path).unwrap().modified().unwrap() > long_ago);
    bridge.stop();
    assert_eq!(*page_cache.dropped.lock().unwrap(), [(file, long_ago)], "the pages of the file alone, once, while it still had its old time");
}

/// A sync mark or a pin that changed is a touch of the file or the directory, so that who
/// watches it reads the mark again, and no change of content: nothing the kernel cached
/// is dropped for it.
#[test]
fn a_change_of_the_marks_touches_a_file_and_a_directory_and_drops_no_cached_page() {
    let fixture = ItemFixture::new();
    let docs = fixture.show_onedrive_directory("docs").local_item_id;
    let file = fixture.show_onedrive_file("docs/a.txt", b"hello").local_item_id;
    let watched = tempfile::tempdir().unwrap();
    let (directory_path, path) = (watched.path().join("docs"), watched.path().join("docs/a.txt"));
    fs::create_dir(&directory_path).unwrap();
    fs::write(&path, b"").unwrap();
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(&path).unwrap().set_modified(long_ago).unwrap();
    fs::File::open(&directory_path).unwrap().set_modified(long_ago).unwrap();
    let page_cache = Arc::new(RecordedPageCache { path: path.clone(), dropped: Default::default() });
    let bridge = Arc::new(ItemInotifyBridge::new(watched.path().to_owned()));
    bridge.start(Arc::clone(&fixture.read_connections), Some(Arc::clone(&page_cache) as Arc<dyn KernelPageCache>)).unwrap();
    bridge.announce(vec![ItemAnnouncement::MarksChanged { item: file, place: place_in(docs, "a.txt") }, ItemAnnouncement::MarksChanged { item: docs, place: place_in(LocalItemId::ROOT, "docs") }]);
    let is_touched = |path: &std::path::Path| fs::metadata(path).unwrap().modified().unwrap() > long_ago;
    wait_until("the file and the directory are touched", || is_touched(&path) && is_touched(&directory_path));
    bridge.stop();
    assert_eq!(*page_cache.dropped.lock().unwrap(), [], "the content did not change");
}

#[test]
fn an_announcement_in_a_directory_the_folder_no_longer_shows_is_skipped_and_the_next_performed() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("shown.txt", b"hello").local_item_id;
    let listed = fixture.list_onedrive_file("listed.txt", b"not shown").local_item_id;
    let (bridge, watched) = start_bridge(&fixture);
    let gone = LocalItemId::new(987_654);
    bridge.announce(vec![
        // Its directory has no row: the name went with the directory.
        ItemAnnouncement::Appeared { item: shown, place: place_in(gone, "inside.txt") },
        ItemAnnouncement::Vanished { item: shown, former_place: place_in(gone, "inside.txt"), kind: ItemKind::File },
        // Its item is not shown, or has no row: nothing of a kind to make.
        ItemAnnouncement::Appeared { item: listed, place: place_in(LocalItemId::ROOT, "listed.txt") },
        ItemAnnouncement::Appeared { item: gone, place: place_in(LocalItemId::ROOT, "gone.txt") },
        ItemAnnouncement::Appeared { item: shown, place: place_in(LocalItemId::ROOT, "shown.txt") },
    ]);
    wait_until("the last name appears", || watched.path().join("shown.txt").exists());
    assert_eq!(fs::read_dir(watched.path()).unwrap().count(), 1, "nothing else was made");
    bridge.stop();
}
