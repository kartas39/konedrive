//! The kernel's requests over the fixture's leaves, with no kernel and no mount: each
//! request is asked on the fixture's mount-like thread, as a thread of the mount asks, and
//! what it replies is taken from the reply moved into it.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fuser::FileType;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::{ContentPresence, ContentVersion, ItemPlace};

use super::*;
use crate::folder::disk::HOLDING;
use crate::fuse::item_attributes::to_node_id;
use crate::fuse::item_download_landing::ItemDownloadLanding;
use crate::fuse::item_fixture::{onedrive_id_of, ContentRefusal, ItemFixture, DOWNLOAD_AFTER, START_TIME};
use crate::fuse::mount_take_down::TakeDownCause;
use crate::status::report::Report;

const ROOT: LocalItemId = LocalItemId::ROOT;
/// No thread of the kernel: a request that is nobody's own.
const A_PROGRAM: u32 = 0;
/// No row has this id.
const NO_ITEM_ID: i64 = 999;

/// The requests over the fixture's leaves.
struct Requests {
    requests: Arc<ItemKernelRequests>,
    write_operations: Arc<ItemWriteOperations>,
    fixture: ItemFixture,
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

impl Requests {
    fn new() -> Self {
        let fixture = ItemFixture::new();
        let bridge = Arc::clone(&fixture.inotify_bridge);
        Self::over(fixture, bridge)
    }

    /// The requests with `inotify_bridge` as the bridge whose own requests they tell.
    fn over(fixture: ItemFixture, inotify_bridge: Arc<ItemInotifyBridge>) -> Self {
        let source = fixture.content_source();
        Self::downloading_from(fixture, inotify_bridge, source)
    }

    /// The requests whose downloads fetch through `source`; a range is fetched through
    /// the fixture's own source, as ever.
    fn downloading_from(fixture: ItemFixture, inotify_bridge: Arc<ItemInotifyBridge>, source: Arc<dyn crate::hydration::source::ContentSource>) -> Self {
        let lookup = Arc::new(ItemLookup::new(
            Arc::clone(&fixture.read_connections),
            Arc::clone(&fixture.place_changes),
            Arc::clone(&fixture.item_handles),
            Arc::clone(&fixture.download_states),
            Arc::clone(&fixture.access_mode_switch),
            Arc::clone(&fixture.upload_rules),
            Arc::clone(&fixture.running_steps),
            Arc::clone(&fixture.published_plan),
            Arc::clone(&fixture.backing_directory),
            START_TIME,
        ));
        let write_operations = Arc::new(ItemWriteOperations::new(
            Arc::clone(&fixture.place_changes),
            Arc::clone(&fixture.backing_directory),
            fixture.store.clone(),
            Arc::clone(&fixture.read_connections),
            Arc::clone(&fixture.item_handles),
            Arc::clone(&fixture.download_states),
            Arc::clone(&fixture.upload_rules),
            Arc::clone(&fixture.access_mode_switch),
            Arc::clone(&fixture.running_steps),
            Arc::clone(&fixture.sync_plan_wake),
            Arc::clone(&fixture.mount_take_down),
        ));
        let landing = Arc::new(ItemDownloadLanding::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.running_steps), Arc::clone(&fixture.mount_take_down)));
        let downloader = Arc::new(ItemDownloader::new(
            Arc::clone(&fixture.backing_directory),
            Arc::clone(&fixture.download_states),
            landing,
            Arc::clone(&fixture.inotify_bridge),
            Arc::clone(&fixture.write_operation_thread),
            source,
            Arc::clone(&fixture.pool),
            Arc::clone(&fixture.split_download_share),
            Report::nowhere(),
            fixture.folder(),
            fixture.runtime(),
        ));
        let free_up = Arc::new(ItemFreeUp::new(Arc::clone(&fixture.place_changes), fixture.store.clone(), Arc::clone(&fixture.item_handles), Arc::clone(&fixture.download_states), Arc::clone(&fixture.running_steps)));
        let requests = Arc::new(ItemKernelRequests::new(
            lookup,
            Arc::clone(&fixture.read_connections),
            Arc::clone(&fixture.place_changes),
            Arc::clone(&fixture.item_handles),
            Arc::clone(&write_operations),
            Arc::clone(&fixture.write_operation_thread),
            inotify_bridge,
            downloader,
            Arc::clone(&fixture.download_states),
            Arc::clone(&fixture.range_fetcher),
            free_up,
            Arc::clone(&fixture.access_mode_switch),
            fixture.store.clone(),
            fixture.runtime(),
        ));
        Requests { requests, write_operations, fixture }
    }

    /// Asks on the mount-like thread what is answered where it is asked.
    fn ask<T: Send + 'static>(&self, question: impl FnOnce(&ItemKernelRequests) -> T + Send + 'static) -> T {
        let requests = Arc::clone(&self.requests);
        self.fixture.run_on_mount_like_thread(move || question(&requests))
    }

    /// Asks on the mount-like thread what is answered through a reply moved into a job,
    /// and waits for the reply.
    fn queue<T: Send + 'static>(&self, question: impl FnOnce(&ItemKernelRequests, ReplyTo<T>) + Send + 'static) -> Result<T, Errno> {
        let (tell, told) = mpsc::channel();
        self.ask(move |requests| question(requests, Box::new(move |answer| drop(tell.send(answer)))));
        told.recv_timeout(Duration::from_secs(20)).expect("the request is answered")
    }

    /// Runs what a request does on the runtime, with its reply, and waits for the reply.
    fn fill<T: Send + 'static, F: std::future::Future<Output = ()>>(&self, question: impl FnOnce(Arc<ItemKernelRequests>, ReplyTo<T>) -> F) -> Result<T, Errno> {
        let (tell, told) = mpsc::channel();
        self.fixture.runtime().block_on(question(Arc::clone(&self.requests), Box::new(move |answer| drop(tell.send(answer)))));
        told.recv_timeout(Duration::from_secs(20)).expect("the request is answered")
    }

    /// Every job queued on the write-operation thread so far has run: a release's among
    /// them.
    fn wait_for_queued_jobs(&self) {
        self.fixture.run_on_write_operation_thread(|| ());
    }

    fn id(&self, path: &str) -> LocalItemId {
        self.fixture.find_item_at(path).unwrap_or_else(|| panic!("the folder shows nothing at {path:?}")).local_item_id
    }

    fn row(&self, item: LocalItemId) -> Item {
        self.fixture.find_item(item).unwrap_or_else(|| panic!("item {item} has no row"))
    }

    fn make_as(&self, path: &str, kind: MadeKind, thread_id: u32) -> Result<MadeEntry, Errno> {
        let (directory, name) = split(path);
        let (directory, name) = (self.id(directory), OsString::from(name));
        self.queue(move |requests, reply| requests.queue_make(directory, &name, kind, thread_id, reply))
    }

    fn make(&self, path: &str, kind: MadeKind) -> Result<MadeEntry, Errno> {
        self.make_as(path, kind, A_PROGRAM)
    }

    fn remove_as(&self, path: &str, asked_kind: ItemKind, thread_id: u32) -> Result<(), Errno> {
        let (directory, name) = split(path);
        let (directory, name) = (self.id(directory), OsString::from(name));
        self.queue(move |requests, reply| requests.queue_remove(directory, &name, asked_kind, thread_id, reply))
    }

    fn remove(&self, path: &str, asked_kind: ItemKind) -> Result<(), Errno> {
        self.remove_as(path, asked_kind, A_PROGRAM)
    }

    fn rename_as(&self, from: &str, to: &str, mode: RenameMode) -> Result<(), Errno> {
        let ((directory, name), (new_directory, new_name)) = (split(from), split(to));
        let (directory, name, new_directory, new_name) = (self.id(directory), OsString::from(name), self.id(new_directory), OsString::from(new_name));
        self.queue(move |requests, reply| requests.queue_rename(directory, &name, new_directory, &new_name, mode, reply))
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.rename_as(from, to, RenameMode::Replace)
    }

    fn lookup(&self, directory: LocalItemId, name: &str) -> Result<LookupAnswer, Errno> {
        let name = OsString::from(name);
        self.ask(move |requests| requests.answer_lookup(directory, &name, A_PROGRAM))
    }

    fn getattr(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        self.ask(move |requests| requests.answer_getattr(node, fh))
    }

    fn open_to_read(&self, node: LocalItemId) -> Result<OpenedFile, Errno> {
        self.ask(move |requests| requests.answer_open_to_read(node))
    }

    /// An open for writing as the switch makes it: decided on the mount's thread, then
    /// queued at once or after the download.
    fn open_to_write(&self, node: LocalItemId) -> Result<OpenedFile, Errno> {
        match self.ask(move |requests| requests.decide_open_to_write(node))? {
            FileToFill::Nothing => self.queue(move |requests, reply| requests.queue_open_to_write(node, reply)),
            FileToFill::Download(file) => self.fill(|requests, reply| async move { requests.fill_and_open_to_write(node, file, reply).await }),
        }
    }

    /// A truncation as the switch makes it.
    fn truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64) -> Result<FileAttr, Errno> {
        match self.ask(move |requests| requests.decide_truncate(node, size))? {
            FileToFill::Nothing => self.queue(move |requests, reply| requests.queue_truncate(node, fh, size, reply)),
            fill => self.fill(|requests, reply| async move { requests.fill_and_truncate(node, fh, size, fill, reply).await }),
        }
    }

    fn set_time_as(&self, node: LocalItemId, fh: Option<Fh>, mtime: i64, thread_id: u32) -> Result<FileAttr, Errno> {
        self.queue(move |requests, reply| requests.queue_set_modified_time(node, fh, mtime, 7, thread_id, reply))
    }

    fn write(&self, fh: Fh, offset: u64, bytes: &'static [u8]) -> Result<u32, Errno> {
        self.ask(move |requests| requests.answer_write(fh, offset, bytes))
    }

    /// A release, and the job it queued run.
    fn release(&self, fh: Fh) {
        self.ask(move |requests| requests.answer_release(fh));
        self.wait_for_queued_jobs();
    }

    /// A part of a directory reading as the switch makes it: the items of the listing
    /// from `offset` on.
    fn readdir(&self, node: LocalItemId, fh: Fh, offset: usize) -> Result<Vec<Item>, Errno> {
        let listed = self.ask(move |requests| requests.answer_readdir(node, fh, offset))?;
        Ok(listed.iter().skip(offset).cloned().collect())
    }

    /// Waits until no download of `item` runs, for no longer than twenty seconds.
    fn wait_until_not_downloading(&self, item: LocalItemId) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.fixture.download_states.is_downloading(item) {
            assert!(Instant::now() < deadline, "the download of item {item} did not end");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn decide_read(&self, fh: Fh, offset: u64, size: u32) -> Result<ReadAnswer, Errno> {
        self.ask(move |requests| requests.decide_read(fh, offset, size))
    }

    fn fetch(&self, range: RangeToFetch) -> Result<Vec<u8>, Errno> {
        let requests = Arc::clone(&self.requests);
        self.fixture.runtime().block_on(async move { requests.fetch_read_range(range).await })
    }

    /// A read as the switch makes it: decided on the mount's thread, the range fetched
    /// on the runtime.
    fn read(&self, fh: Fh, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        match self.decide_read(fh, offset, size)? {
            ReadAnswer::Bytes(bytes) => Ok(bytes),
            ReadAnswer::Range(range) => self.fetch(range),
        }
    }

    fn list_fetched_ranges(&self) -> Vec<(u64, Option<u64>)> {
        self.fixture.fixture_content_source().list_fetches().into_iter().map(|fetch| (fetch.from, fetch.end)).collect()
    }

    /// Waits until the content of `item` is on disk, for no longer than twenty seconds.
    fn wait_until_on_disk(&self, item: LocalItemId) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.row(item).content_presence != ContentPresence::OnDisk || self.fixture.download_states.is_downloading(item) {
            assert!(Instant::now() < deadline, "the download of item {item} did not land");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn local_content(row: &Item) -> ContentVersion {
    row.local.as_ref().and_then(|local| local.content.clone()).expect("a file the folder shows")
}

fn local_name(row: &Item) -> OsString {
    row.local.as_ref().and_then(|local| local.place.as_ref()).expect("an item at a place").name.clone()
}

fn seconds_of(time: std::time::SystemTime) -> i64 {
    time.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[test]
fn a_range_of_a_file_not_on_disk_per_case() {
    // At or past the end: nothing, whatever a download wrote.
    assert_eq!(decide_range_origin(10, None, 10, 4), RangeOrigin::Nothing);
    assert_eq!(decide_range_origin(10, Some(10), 12, 4), RangeOrigin::Nothing);
    assert_eq!(decide_range_origin(0, None, 0, 4), RangeOrigin::Nothing);
    // The read ends at the content's end.
    assert_eq!(decide_range_origin(10, None, 4, 100), RangeOrigin::OneDrive { from: 4, end: 10 });
    assert_eq!(decide_range_origin(10, None, 0, 4), RangeOrigin::OneDrive { from: 0, end: 4 });
    // A running download: only what it has surely written.
    let size = 8 * WRITTEN_BEHIND;
    assert_eq!(decide_range_origin(size, Some(4096 + WRITTEN_BEHIND), 0, 4096), RangeOrigin::RunningDownload { from: 0, end: 4096 });
    assert_eq!(decide_range_origin(size, Some(4095 + WRITTEN_BEHIND), 0, 4096), RangeOrigin::OneDrive { from: 0, end: 4096 });
    assert_eq!(decide_range_origin(size, Some(0), 0, 4096), RangeOrigin::OneDrive { from: 0, end: 4096 }, "a download in parts says nothing of its file");
}

#[test]
fn a_removal_the_bridge_announces_per_case() {
    assert_eq!(decide_announced_removal(Err(Errno::ENOENT)), Ok(()), "the name is gone: done");
    assert_eq!(decide_announced_removal(Err(Errno::ESTALE)), Ok(()), "its directory is gone: done");
    assert_eq!(decide_announced_removal(Ok(())), Err(Errno::EROFS), "the folder shows the name: nothing is removed for the bridge");
    assert_eq!(decide_announced_removal(Err(Errno::EIO)), Err(Errno::EIO));
}

#[test]
fn the_reading_requests_answer_what_the_rows_show() {
    let mount = Requests::new();
    let docs = mount.fixture.show_onedrive_directory("docs").local_item_id;
    let file = mount.fixture.show_onedrive_file("docs/a.txt", b"hello").local_item_id;
    mount.fixture.land_copy(file, b"hello");

    let found = mount.lookup(ROOT, "docs").unwrap();
    assert_eq!((found.attributes().ino, found.attributes().kind, found.lifetime()), (to_node_id(docs), FileType::Directory, Duration::from_secs(1)));
    let found = mount.lookup(docs, "a.txt").unwrap();
    assert_eq!((found.attributes().ino, found.attributes().size), (to_node_id(file), 5));
    assert_eq!(mount.lookup(docs, "none.txt").err(), Some(Errno::ENOENT));
    assert_eq!(mount.lookup(file, "a.txt").err(), Some(Errno::ENOTDIR));
    assert_eq!(mount.getattr(file, None).unwrap().size, 5);

    assert_eq!(mount.ask(move |requests| requests.answer_opendir(file)).err(), Some(Errno::ENOTDIR));
    let fh = mount.ask(move |requests| requests.answer_opendir(docs)).unwrap();
    let listed = mount.readdir(docs, fh, 0).unwrap();
    assert_eq!(listed.iter().map(local_name).collect::<Vec<_>>(), ["a.txt"]);
    let described = mount.ask(move |requests| requests.describe_listed_item(listed[0].clone())).unwrap();
    assert_eq!((described.attributes.ino, described.attributes.size), (to_node_id(file), 5));

    // A reading in parts reads the listing of its start; a reading from the start lists anew.
    mount.make("docs/b.txt", MadeKind::File).unwrap();
    assert_eq!(mount.readdir(docs, fh, 1).unwrap().len(), 0);
    let listed = mount.readdir(docs, fh, 0).unwrap();
    assert_eq!(listed.iter().map(local_name).collect::<Vec<_>>(), ["a.txt", "b.txt"]);
    assert_eq!(mount.readdir(docs, fh, 1).unwrap().iter().map(local_name).collect::<Vec<_>>(), ["b.txt"]);
    mount.ask(move |requests| requests.answer_releasedir(fh));
    assert!(mount.fixture.item_handles.find_listed_items(fh).is_none(), "the directory is closed");

    let item_id = mount.ask(move |requests| requests.answer_getxattr(file, OsStr::new(XATTR_ITEM_ID))).unwrap();
    assert_eq!(String::from_utf8(item_id).unwrap(), onedrive_id_of("docs/a.txt"));
    assert!(mount.ask(move |requests| requests.answer_listxattr(file)).unwrap().contains(&XATTR_ITEM_ID));
    assert_eq!(mount.ask(move |requests| requests.answer_getxattr(file, OsStr::new("user.other"))).err(), Some(Errno::ENODATA));
}

#[test]
fn a_request_by_a_node_whose_row_is_gone_is_stale() {
    let mount = Requests::new();
    let stale = Some(Errno::ESTALE);
    assert_eq!(mount.lookup(LocalItemId::new(NO_ITEM_ID), "a.txt").err(), stale);
    assert_eq!(mount.getattr(LocalItemId::new(NO_ITEM_ID), None).err(), stale);
    assert_eq!(mount.ask(|requests| requests.answer_opendir(LocalItemId::new(NO_ITEM_ID))).err(), stale);
    let fh = mount.ask(|requests| requests.answer_opendir(ROOT)).unwrap();
    assert_eq!(mount.readdir(LocalItemId::new(NO_ITEM_ID), fh, 0).err(), stale);
    assert_eq!(mount.ask(|requests| requests.answer_getxattr(LocalItemId::new(NO_ITEM_ID), OsStr::new(XATTR_ITEM_ID))).err(), stale);
    assert_eq!(mount.ask(|requests| requests.answer_listxattr(LocalItemId::new(NO_ITEM_ID))).err(), stale);
    assert_eq!(mount.open_to_read(LocalItemId::new(NO_ITEM_ID)).err(), stale);
    assert_eq!(mount.open_to_write(LocalItemId::new(NO_ITEM_ID)).err(), stale);
    assert_eq!(mount.queue(|requests, reply| requests.queue_open_to_write(LocalItemId::new(NO_ITEM_ID), reply)).err(), stale);
    assert_eq!(mount.queue(|requests, reply| requests.queue_open_to_overwrite(LocalItemId::new(NO_ITEM_ID), reply)).err(), stale);
    assert_eq!(mount.truncate(LocalItemId::new(NO_ITEM_ID), None, 0).err(), stale);
    assert_eq!(mount.queue(|requests, reply| requests.queue_truncate(LocalItemId::new(NO_ITEM_ID), None, 0, reply)).err(), stale);
    assert_eq!(mount.set_time_as(LocalItemId::new(NO_ITEM_ID), None, 5, A_PROGRAM).err(), stale);

    // An item of OneDrive that was removed here keeps its row, which shows nothing.
    let removed = mount.fixture.show_onedrive_file("gone.txt", b"gone").local_item_id;
    mount.remove("gone.txt", ItemKind::File).unwrap();
    assert!(mount.fixture.find_item(removed).is_some(), "the removal is still to be sent");
    assert_eq!(mount.getattr(removed, None).err(), stale);
    assert_eq!(mount.open_to_read(removed).err(), stale);
}

#[test]
fn a_request_with_a_handle_whose_row_is_gone_is_answered_from_the_handles_file() {
    let mount = Requests::new();
    let made = mount.make("n.txt", MadeKind::FileOpenedForWriting).unwrap();
    let (node, fh) = (made.entry.item.local_item_id, made.fh);
    assert_eq!(mount.write(fh, 0, b"abcdef"), Ok(6));
    mount.remove("n.txt", ItemKind::File).unwrap();
    assert!(mount.fixture.find_item(node).is_none());

    assert_eq!(mount.getattr(node, None).err(), Some(Errno::ESTALE));
    assert_eq!(mount.getattr(node, Some(fh)).unwrap().size, 6);
    assert_eq!(mount.write(fh, 6, b"gh"), Ok(2), "the handle keeps working");
    assert_eq!(mount.read(fh, 0, 100).unwrap(), b"abcdefgh");
    assert_eq!(mount.truncate(node, Some(fh), 3).unwrap().size, 3);
    let touched = mount.set_time_as(node, Some(fh), 1_650_000_000, A_PROGRAM).unwrap();
    assert_eq!((touched.size, seconds_of(touched.mtime)), (3, 1_650_000_000));

    mount.release(fh);
    assert!(mount.fixture.item_handles.find_file(fh).is_none(), "the writer is closed");
    assert!(mount.fixture.find_item(node).is_none(), "the last close writes no row for an item that is gone");
    assert!(mount.fixture.find_item_at("n.txt").is_none());
    mount.fixture.assert_rows_and_disk_agree();
}

/// Each changing request is refused `EROFS`; none of them is queued or downloads.
fn assert_every_change_is_refused(mount: &Requests, file: LocalItemId, not_on_disk: LocalItemId) {
    let read_only = Some(Errno::EROFS);
    assert_eq!(mount.make("new", MadeKind::Directory).err(), read_only);
    assert_eq!(mount.make("new.txt", MadeKind::File).err(), read_only);
    assert_eq!(mount.make("new.txt", MadeKind::FileOpenedForWriting).err(), read_only);
    assert_eq!(mount.remove("a.txt", ItemKind::File).err(), read_only);
    assert_eq!(mount.rename("a.txt", "b.txt").err(), read_only);
    assert_eq!(mount.rename_as("a.txt", "b.txt", RenameMode::Exchange).err(), read_only, "as today: read-only before the mode is looked at");
    assert_eq!(mount.ask(move |requests| requests.decide_open_to_write(not_on_disk)).err(), read_only);
    assert_eq!(mount.queue(move |requests, reply| requests.queue_open_to_write(file, reply)).err(), read_only);
    assert_eq!(mount.queue(move |requests, reply| requests.queue_open_to_overwrite(file, reply)).err(), read_only);
    assert_eq!(mount.ask(move |requests| requests.decide_truncate(not_on_disk, 3)).err(), read_only);
    assert_eq!(mount.queue(move |requests, reply| requests.queue_truncate(file, None, 0, reply)).err(), read_only);
    assert_eq!(mount.set_time_as(file, None, 5, A_PROGRAM).err(), read_only);
    assert_eq!(mount.fixture.write_operation_thread.count_queued_jobs(), 0, "nothing was queued");
    assert!(mount.list_fetched_ranges().is_empty(), "nothing was downloaded for a write the folder will not take");
}

#[test]
fn a_changing_request_in_a_read_only_folder_is_refused_before_it_is_queued() {
    let mount = Requests::new();
    let file = mount.fixture.make_own_file("a.txt", b"a").local_item_id;
    let not_on_disk = mount.fixture.show_onedrive_file("far.txt", b"far away").local_item_id;
    mount.fixture.access_mode_switch.switch_to_read_only();
    // A cycle holds the tree lock: a job of the write-operation thread would wait for it,
    // and its request with it.
    let held = mount.fixture.tree_lock.try_lock().unwrap();
    assert_every_change_is_refused(&mount, file, not_on_disk);
    drop(held);
    assert_eq!(mount.fixture.read_disk_file("a.txt").as_deref(), Some(&b"a"[..]));
}

#[test]
fn after_the_take_down_was_asked_every_change_is_refused_and_reading_goes_on() {
    let mount = Requests::new();
    let file = mount.fixture.make_own_file("a.txt", b"a").local_item_id;
    let not_on_disk = mount.fixture.show_onedrive_file("far.txt", b"far away").local_item_id;
    let open = mount.fixture.make_own_file("open.txt", b"open").local_item_id;
    let writer = mount.open_to_write(open).unwrap().fh;

    mount.fixture.mount_take_down.ask(TakeDownCause::UndoFailed { path: "somewhere".into() });
    assert_every_change_is_refused(&mount, file, not_on_disk);

    // Reading goes on, from the disk and from OneDrive.
    assert_eq!(mount.lookup(ROOT, "a.txt").unwrap().attributes().size, 1);
    let reader = mount.open_to_read(file).unwrap().fh;
    assert_eq!(mount.read(reader, 0, 10).unwrap(), b"a");
    let far = mount.open_to_read(not_on_disk).unwrap().fh;
    assert_eq!(mount.read(far, 0, 3).unwrap(), b"far");

    // A file that was open goes on being written, and its last close is written.
    assert_eq!(mount.write(writer, 4, b"ed"), Ok(2));
    mount.release(writer);
    assert_eq!(local_content(&mount.row(open)).size(), 6);
    assert!(mount.row(open).content_before_write.is_none());
}

#[test]
fn a_read_of_a_backing_file_is_answered_on_the_mounts_thread() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("a.txt", b"hello world").local_item_id;
    mount.fixture.land_copy(file, b"hello world");
    let opened = mount.open_to_read(file).unwrap();
    assert!(opened.keeps_cache);
    assert!(matches!(mount.decide_read(opened.fh, 6, 100), Ok(ReadAnswer::Bytes(bytes)) if bytes == b"world"), "the bytes, and fewer at the end");
    assert!(matches!(mount.decide_read(opened.fh, 11, 100), Ok(ReadAnswer::Bytes(bytes)) if bytes.is_empty()));
    assert!(mount.list_fetched_ranges().is_empty(), "nothing is asked of OneDrive");
    mount.release(opened.fh);
    assert!(mount.fixture.item_handles.find_file(opened.fh).is_none(), "the reader is closed");
    assert_eq!(mount.decide_read(opened.fh, 0, 1).err(), Some(Errno::EBADF));
}

#[test]
fn a_read_of_a_file_not_on_disk_reads_its_rows_on_the_mounts_thread_and_fetches_on_the_runtime() {
    let mount = Requests::new();
    let content: Vec<u8> = (0..DOWNLOAD_AFTER as usize + (DOWNLOAD_AFTER as usize / 2)).map(|at| (at % 251) as u8).collect();
    let row = mount.fixture.show_onedrive_file("big.bin", &content);
    let file = row.local_item_id;
    let opened = mount.open_to_read(file).unwrap();
    assert!(matches!(mount.fixture.item_handles.find_file(opened.fh), Some(OpenItemFile::ReadingNotOnDisk { item, .. }) if item == file));
    assert!(mount.list_fetched_ranges().is_empty(), "opening downloads nothing");

    // Decided on the mount's thread: the range, as values.
    let Ok(ReadAnswer::Range(range)) = mount.decide_read(opened.fh, 0, 4096) else { panic!("a range to fetch") };
    assert_eq!((range.item, range.from, range.end), (file, 0, 4096));
    assert_eq!(Some(&range.source.onedrive_item), row.onedrive_item.as_ref());
    assert_eq!(range.source.content, local_content(&row));
    assert_eq!(range.download_at_threshold, ItemToDownload { item: file, onedrive_item: row.onedrive_item.clone().unwrap(), content: local_content(&row) });
    assert!(mount.list_fetched_ranges().is_empty(), "the mount's thread fetches nothing");

    // Fetched on the runtime. Below the threshold no download is started.
    assert_eq!(mount.fetch(range).unwrap(), &content[..4096]);
    assert_eq!(mount.list_fetched_ranges(), [(0, Some(4096))]);
    assert!(!mount.fixture.download_states.is_downloading(file));
    assert_eq!(mount.row(file).content_presence, ContentPresence::NotOnDisk);

    // The threshold reached: the whole file is downloaded, and no read waits for it.
    assert_eq!(mount.read(opened.fh, 4096, DOWNLOAD_AFTER as u32).unwrap(), &content[4096..4096 + DOWNLOAD_AFTER as usize]);
    mount.wait_until_on_disk(file);
    assert_eq!(mount.fixture.read_disk_file("big.bin").as_deref(), Some(&content[..]));

    // The handle goes on from the backing file.
    let fetched = mount.list_fetched_ranges().len();
    assert!(matches!(mount.decide_read(opened.fh, content.len() as u64 - 3, 100), Ok(ReadAnswer::Bytes(bytes)) if bytes == content[content.len() - 3..]));
    assert!(matches!(mount.fixture.item_handles.find_file(opened.fh), Some(OpenItemFile::ReadingBackingFile { item, .. }) if item == file));
    assert_eq!(mount.list_fetched_ranges().len(), fetched);
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_read_of_a_small_file_not_on_disk_downloads_it_once_all_of_it_was_read() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("small.txt", b"0123456789").local_item_id;
    let fh = mount.open_to_read(file).unwrap().fh;
    assert_eq!(mount.read(fh, 0, 4).unwrap(), b"0123");
    assert!(matches!(mount.decide_read(fh, 10, 4), Ok(ReadAnswer::Bytes(bytes)) if bytes.is_empty()), "nothing past the end, and nothing fetched for it");
    assert!(!mount.fixture.download_states.is_downloading(file));
    assert_eq!(mount.row(file).content_presence, ContentPresence::NotOnDisk);
    assert_eq!(mount.read(fh, 4, 100).unwrap(), b"456789", "the read ends at the content's end");
    mount.wait_until_on_disk(file);
    assert_eq!(mount.fixture.read_disk_file("small.txt").as_deref(), Some(&b"0123456789"[..]));
}

#[test]
fn a_read_through_a_handle_whose_content_onedrive_changed_is_stale() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("a.txt", b"the first content").local_item_id;
    let fh = mount.open_to_read(file).unwrap().fh;
    assert_eq!(mount.read(fh, 0, 3).unwrap(), b"the");

    // OneDrive has another content, and the rows do not say so yet: the range's answer does.
    mount.fixture.serve_content(file, b"another content, longer");
    assert_eq!(mount.read(fh, 0, 3).err(), Some(Errno::ESTALE));
    assert!(!mount.fixture.download_states.is_downloading(file), "nothing of the other content is counted or downloaded");

    // The row shows nothing any more: the handle has no file to be answered from.
    let other = mount.fixture.show_onedrive_file("b.txt", b"second").local_item_id;
    let fh = mount.open_to_read(other).unwrap().fh;
    mount.remove("b.txt", ItemKind::File).unwrap();
    assert_eq!(mount.decide_read(fh, 0, 3).err(), Some(Errno::ESTALE));
}

#[test]
fn an_open_of_a_row_that_says_on_disk_over_no_file_mends_a_clean_row_and_reads_from_onedrive() {
    let mount = Requests::new();
    let clean = mount.fixture.show_onedrive_file("clean.txt", b"a clean copy").local_item_id;
    mount.fixture.land_copy(clean, b"a clean copy");
    std::fs::remove_file(mount.fixture.disk_path("clean.txt")).unwrap();
    let opened = mount.open_to_read(clean).unwrap();
    assert!(matches!(mount.fixture.item_handles.find_file(opened.fh), Some(OpenItemFile::ReadingNotOnDisk { item, .. }) if item == clean));
    assert_eq!(mount.row(clean).content_presence, ContentPresence::NotOnDisk, "the row is mended");
    assert_eq!(mount.read(opened.fh, 2, 5).unwrap(), b"clean");

    // A file changed here has nothing to be read from: the row is left for the check at start.
    let own = mount.fixture.make_own_file("own.txt", b"the user's").local_item_id;
    std::fs::remove_file(mount.fixture.disk_path("own.txt")).unwrap();
    assert_eq!(mount.open_to_read(own).err(), Some(Errno::ENOENT));
    assert_eq!(mount.row(own).content_presence, ContentPresence::OnDisk);
}

#[test]
fn a_file_not_on_disk_is_downloaded_before_it_is_opened_for_writing() {
    let mount = Requests::new();
    let row = mount.fixture.show_onedrive_file("w.txt", b"0123456789");
    let file = row.local_item_id;
    assert_eq!(mount.ask(move |requests| requests.decide_open_to_write(file)), Ok(FileToFill::Download(ItemToDownload { item: file, onedrive_item: row.onedrive_item.clone().unwrap(), content: local_content(&row) })));

    let opened = mount.open_to_write(file).unwrap();
    assert!(!opened.keeps_cache);
    assert_eq!(mount.fixture.read_disk_file("w.txt").as_deref(), Some(&b"0123456789"[..]), "downloaded first");
    let open_row = mount.row(file);
    assert_eq!((open_row.content_presence, open_row.content_before_write.as_ref()), (ContentPresence::OnDisk, Some(&local_content(&row))), "the first writer's open is in the row");
    assert_eq!(mount.ask(move |requests| requests.decide_open_to_write(file)), Ok(FileToFill::Nothing), "on disk now");

    mount.release(opened.fh);
    let closed = mount.row(file);
    assert_eq!((local_content(&closed), closed.content_before_write, closed.unsent_since), (local_content(&row), None, None), "closed with nothing written: the content of before");
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_download_that_fails_refuses_the_open_for_writing_and_the_truncation_with_its_errno() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("w.txt", b"0123456789").local_item_id;
    // OneDrive does not have the content any more: stale, as the waiters of a download hear it.
    mount.fixture.refuse_content(file, Some(ContentRefusal::NotFound));
    assert_eq!(mount.open_to_write(file).err(), Some(Errno::ESTALE));
    assert_eq!(mount.truncate(file, None, 3).err(), Some(Errno::ESTALE));
    // The network: the request failed.
    mount.fixture.refuse_content(file, Some(ContentRefusal::Transient));
    assert_eq!(mount.open_to_write(file).err(), Some(Errno::EIO));
    let row = mount.row(file);
    assert!(row.content_presence == ContentPresence::NotOnDisk && row.content_before_write.is_none() && row.unsent_since.is_none(), "no row was written: {row:?}");
    assert!(!mount.fixture.item_handles.is_open_for_writing(file) && mount.fixture.read_disk_file("w.txt").is_none());
    // Cut to nothing, it needs no download.
    assert_eq!(mount.truncate(file, None, 0).unwrap().size, 0);
}

#[test]
fn an_append_is_put_at_the_size_the_writers_file_has() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("log.txt", b"line one\n").local_item_id;
    mount.fixture.land_copy(file, b"line one\n");
    let fh = mount.open_to_write(file).unwrap().fh;
    // The kernel puts an append at the size getattr answered: the writer's file's, while
    // the row keeps the size of before the open.
    for line in [&b"line two\n"[..], &b"line three\n"[..]] {
        let end = mount.getattr(file, None).unwrap().size;
        assert_eq!(mount.write(fh, end, line), Ok(line.len() as u32));
        assert_eq!(mount.getattr(file, None).unwrap().size, end + line.len() as u64, "the backing file's size after each write");
        assert_eq!(local_content(&mount.row(file)).size(), 9, "no row is touched per write");
    }
    assert_eq!(mount.lookup(ROOT, "log.txt").unwrap().attributes().size, 29);
    assert_eq!(mount.ask(move |requests| requests.answer_fsync(fh)), Ok(()));
    mount.release(fh);
    assert_eq!(mount.fixture.read_disk_file("log.txt").as_deref(), Some(&b"line one\nline two\nline three\n"[..]));
    let closed = mount.row(file);
    assert!(matches!(local_content(&closed), ContentVersion::ChangedHere { size: 29, .. }) && closed.unsent_since.is_some() && closed.content_before_write.is_none(), "the last close writes the size: {closed:?}");
}

#[test]
fn a_truncation_to_nothing_of_a_file_not_on_disk_downloads_nothing() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("t.txt", b"content").local_item_id;
    assert_eq!(mount.ask(move |requests| requests.decide_truncate(file, 0)), Ok(FileToFill::Nothing));
    assert_eq!(mount.truncate(file, None, 0).unwrap().size, 0);
    assert!(mount.list_fetched_ranges().is_empty());
    let row = mount.row(file);
    assert!(matches!(local_content(&row), ContentVersion::ChangedHere { size: 0, .. }) && row.content_presence == ContentPresence::OnDisk && row.unsent_since.is_some(), "{row:?}");
    assert_eq!(mount.fixture.read_disk_file("t.txt").as_deref(), Some(&b""[..]));
    assert_eq!(mount.getattr(file, None).unwrap().size, 0);
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_truncation_of_a_file_not_on_disk_downloads_it_first() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("t.txt", b"content").local_item_id;
    assert!(matches!(mount.ask(move |requests| requests.decide_truncate(file, 3)), Ok(FileToFill::Download(to_download)) if to_download.item == file));
    assert_eq!(mount.truncate(file, None, 3).unwrap().size, 3);
    assert_eq!(mount.fixture.read_disk_file("t.txt").as_deref(), Some(&b"con"[..]));
    assert_eq!(local_content(&mount.row(file)).size(), 3);
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_opened_to_be_overwritten_is_cut_and_nothing_is_downloaded() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("o.txt", b"the old content").local_item_id;
    let opened = mount.queue(move |requests, reply| requests.queue_open_to_overwrite(file, reply)).unwrap();
    assert!(!opened.keeps_cache);
    assert!(mount.list_fetched_ranges().is_empty());
    assert_eq!(mount.getattr(file, None).unwrap().size, 0);
    assert_eq!(mount.write(opened.fh, 0, b"new"), Ok(3));
    mount.release(opened.fh);
    assert_eq!(mount.fixture.read_disk_file("o.txt").as_deref(), Some(&b"new"[..]));
    let row = mount.row(file);
    assert!(matches!(local_content(&row), ContentVersion::ChangedHere { size: 3, .. }) && row.content_presence == ContentPresence::OnDisk, "{row:?}");
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_created_file_written_and_closed_is_one_unsent_item_with_its_bytes() {
    let mount = Requests::new();
    let made = mount.make("new.txt", MadeKind::FileOpenedForWriting).unwrap();
    let (node, fh) = (made.entry.item.local_item_id, made.fh);
    assert_ne!(fh, Fh::NOT_OPENED);
    assert_eq!((made.entry.attributes.ino, made.entry.attributes.size, made.entry.attributes.kind), (to_node_id(node), 0, FileType::RegularFile));
    assert_eq!(mount.lookup(ROOT, "new.txt").unwrap().attributes().ino, to_node_id(node));

    assert_eq!(mount.write(fh, 0, b"hello"), Ok(5));
    assert_eq!(mount.getattr(node, Some(fh)).unwrap().size, 5);
    assert_eq!(mount.read(fh, 1, 3).unwrap(), b"ell", "a writer's handle reads its file");
    assert_eq!(mount.ask(move |requests| requests.answer_fsync(fh)), Ok(()));
    mount.release(fh);

    let unsent: Vec<Item> = mount.fixture.list_items().into_iter().filter(|row| row.unsent_since.is_some()).collect();
    assert_eq!(unsent.iter().map(|row| row.local_item_id).collect::<Vec<_>>(), [node], "one unsent item");
    assert!(matches!(local_content(&unsent[0]), ContentVersion::ChangedHere { size: 5, .. }) && unsent[0].onedrive_item.is_none() && unsent[0].content_before_write.is_none(), "{:?}", unsent[0]);
    assert_eq!(mount.fixture.read_disk_file("new.txt").as_deref(), Some(&b"hello"[..]));
    assert_eq!(mount.getattr(node, None).unwrap().size, 5);
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn each_request_writes_the_change_its_line_of_the_table_names() {
    let mount = Requests::new();
    // mkdir, mknod: `Made`.
    let directory = mount.make("d", MadeKind::Directory).unwrap();
    assert_eq!((directory.entry.attributes.kind, directory.fh), (FileType::Directory, Fh::NOT_OPENED));
    let file = mount.make("d/f.txt", MadeKind::File).unwrap();
    let (node, place) = (file.entry.item.local_item_id, ItemPlace { parent_local_item_id: directory.entry.item.local_item_id, name: "f.txt".into() });
    assert_eq!((file.entry.attributes.ino, file.entry.attributes.size, file.fh), (to_node_id(node), 0, Fh::NOT_OPENED));
    let made = mount.row(node);
    assert!(made.local.as_ref().unwrap().place.as_ref() == Some(&place) && made.unsent_since.is_some() && made.content_presence == ContentPresence::OnDisk, "{made:?}");
    assert!(mount.fixture.disk_path("d/f.txt").is_file());

    // open (write): `OpenedForWriting`; release with nothing written: `ClosedUnchanged`.
    let fh = mount.open_to_write(node).unwrap().fh;
    assert_eq!(mount.row(node).content_before_write, Some(local_content(&made)));
    mount.release(fh);
    assert_eq!((local_content(&mount.row(node)), mount.row(node).content_before_write), (local_content(&made), None));

    // release after a write: `ClosedChanged`.
    let fh = mount.open_to_write(node).unwrap().fh;
    mount.write(fh, 0, b"four").unwrap();
    mount.release(fh);
    assert_eq!(local_content(&mount.row(node)).size(), 4);

    // setattr (size) with no writer: `Truncated`.
    assert_eq!(mount.truncate(node, None, 2).unwrap().size, 2);
    assert_eq!(local_content(&mount.row(node)).size(), 2);

    // rename: `Moved`; over a file: `SavedOver`.
    mount.rename("d/f.txt", "g.txt").unwrap();
    assert_eq!(mount.id("g.txt"), node);
    assert!(mount.fixture.find_item_at("d/f.txt").is_none() && mount.fixture.disk_path("g.txt").is_file());
    let replaced = mount.make("h.txt", MadeKind::File).unwrap().entry.item.local_item_id;
    mount.rename("g.txt", "h.txt").unwrap();
    assert_eq!(mount.id("h.txt"), node);
    assert!(mount.fixture.find_item(replaced).is_none() && mount.fixture.find_item_at("g.txt").is_none());

    // unlink, rmdir: `Removed`.
    mount.remove("h.txt", ItemKind::File).unwrap();
    mount.remove("d", ItemKind::Directory).unwrap();
    assert!(mount.fixture.find_item(node).is_none() && mount.fixture.find_item_at("d").is_none());
    assert!(!mount.fixture.disk_path("h.txt").exists() && !mount.fixture.disk_path("d").exists());
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn each_refusal_of_a_write_operation_reaches_the_reply_with_its_errno() {
    let mount = Requests::new();
    mount.make("d", MadeKind::Directory).unwrap();
    mount.make("d/f.txt", MadeKind::File).unwrap();
    mount.make("g.txt", MadeKind::File).unwrap();
    let errno_of = |answer: Result<MadeEntry, Errno>| answer.err();

    assert_eq!(errno_of(mount.make("d/f.txt", MadeKind::File)), Some(Errno::EEXIST));
    assert_eq!(errno_of(mount.make("d", MadeKind::Directory)), Some(Errno::EEXIST));
    assert_eq!(errno_of(mount.make("d/f.txt", MadeKind::FileOpenedForWriting)), Some(Errno::EEXIST));
    assert_eq!(errno_of(mount.make(HOLDING, MadeKind::Directory)), Some(Errno::EPERM), "a name the mount hides");
    assert_eq!(mount.queue(|requests, reply| requests.queue_make(LocalItemId::new(NO_ITEM_ID), OsStr::new("x"), MadeKind::File, A_PROGRAM, reply)).err(), Some(Errno::ENOENT));

    assert_eq!(mount.remove("none.txt", ItemKind::File).err(), Some(Errno::ENOENT));
    assert_eq!(mount.remove("d", ItemKind::File).err(), Some(Errno::EISDIR));
    assert_eq!(mount.remove("g.txt", ItemKind::Directory).err(), Some(Errno::ENOTDIR));
    assert_eq!(mount.remove("d", ItemKind::Directory).err(), Some(Errno::ENOTEMPTY));

    assert_eq!(mount.rename_as("g.txt", "d/f.txt", RenameMode::Exchange).err(), Some(Errno::EINVAL));
    assert_eq!(mount.rename_as("g.txt", "d/f.txt", RenameMode::NoReplace).err(), Some(Errno::EEXIST));
    assert_eq!(mount.rename("none.txt", "other.txt").err(), Some(Errno::ENOENT));
    assert_eq!(mount.rename("g.txt", "d").err(), Some(Errno::EISDIR));
    assert_eq!(mount.rename("d", "g.txt").err(), Some(Errno::ENOTDIR));
    assert_eq!(mount.rename("g.txt", HOLDING).err(), Some(Errno::EPERM));

    let directory = mount.id("d");
    assert_eq!(mount.open_to_read(directory).err(), Some(Errno::EISDIR));
    assert_eq!(mount.open_to_write(directory).err(), Some(Errno::EISDIR));
    assert_eq!(mount.truncate(directory, None, 0).err(), Some(Errno::EISDIR));
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_file_renamed_while_it_is_open_is_closed_into_the_row_at_its_new_place() {
    let mount = Requests::new();
    let made = mount.make("a.txt", MadeKind::FileOpenedForWriting).unwrap();
    let (node, fh) = (made.entry.item.local_item_id, made.fh);
    mount.write(fh, 0, b"one").unwrap();
    let reader = mount.open_to_read(node).unwrap().fh;

    mount.rename("a.txt", "b.txt").unwrap();
    assert_eq!(mount.write(fh, 3, b"two"), Ok(3), "the writer keeps working");
    assert_eq!(mount.read(reader, 0, 100).unwrap(), b"onetwo", "and the reader");
    assert_eq!(mount.getattr(node, None).unwrap().size, 6);
    let found = mount.lookup(ROOT, "b.txt").unwrap();
    assert_eq!((found.attributes().ino, found.attributes().size), (to_node_id(node), 6));
    assert_eq!(mount.lookup(ROOT, "a.txt").err(), Some(Errno::ENOENT));

    // A truncation of a file open for writing cuts the writer's file; its reply says that
    // file's size, while the row keeps the size of before the open until the last close.
    assert_eq!(mount.truncate(node, None, 4).unwrap().size, 4);
    assert_eq!(local_content(&mount.row(node)).size(), 0);

    mount.release(reader);
    mount.release(fh);
    assert_eq!(mount.id("b.txt"), node);
    assert_eq!(local_content(&mount.row(node)).size(), 4);
    assert_eq!(mount.fixture.read_disk_file("b.txt").as_deref(), Some(&b"onet"[..]));
    assert!(mount.fixture.read_disk_file("a.txt").is_none());
    mount.fixture.assert_rows_and_disk_agree();
}

#[test]
fn a_directory_read_during_renames_describes_each_listed_item_as_itself() {
    let mount = Requests::new();
    let large = mount.fixture.make_own_file("a", &vec![7u8; 1 << 16]).local_item_id;
    let empty = mount.fixture.make_own_file("b", b"").local_item_id;
    let fh = mount.ask(|requests| requests.answer_opendir(ROOT)).unwrap();
    let listed = mount.readdir(ROOT, fh, 0).unwrap();
    assert_eq!(listed.iter().map(local_name).collect::<Vec<_>>(), ["a", "b"]);

    // Between the listing and its attributes the names change hands: `a` is the empty file's now.
    mount.rename("a", "c").unwrap();
    mount.rename("b", "a").unwrap();

    let described: Vec<ShownEntry> = listed.into_iter().map(|item| mount.ask(move |requests| requests.describe_listed_item(item)).unwrap()).collect();
    let facts: Vec<_> = described.iter().map(|entry| (entry.attributes.ino, entry.attributes.size, entry.attributes.blocks > 0)).collect();
    assert_eq!(facts, [(to_node_id(large), 1 << 16, true), (to_node_id(empty), 0, false)], "each entry has its own item's attributes, the blocks of its own file among them");
    // The next part of the reading is of the same listing.
    assert_eq!(mount.readdir(ROOT, fh, 1).unwrap().iter().map(|item| item.local_item_id).collect::<Vec<_>>(), [empty]);
}

#[test]
fn the_time_set_on_a_file_is_what_its_reply_and_the_next_getattr_say() {
    let mount = Requests::new();
    let file = mount.fixture.make_own_file("a.txt", b"abc").local_item_id;
    let touched = mount.set_time_as(file, None, 1_650_000_000, A_PROGRAM).unwrap();
    assert_eq!((touched.ino, touched.size, seconds_of(touched.mtime)), (to_node_id(file), 3, 1_650_000_000));
    assert_eq!(seconds_of(mount.getattr(file, None).unwrap().mtime), 1_650_000_000);
    assert_eq!(local_content(&mount.row(file)).mtime(), (1_650_000_000, 7));
}

#[test]
fn a_request_by_a_handle_that_is_not_open_is_ebadf() {
    let mount = Requests::new();
    let file = mount.fixture.make_own_file("a.txt", b"abc").local_item_id;
    let reader = mount.open_to_read(file).unwrap().fh;
    assert_eq!(mount.write(reader, 0, b"x").err(), Some(Errno::EBADF), "a reader's handle may not write");
    assert_eq!(mount.write(Fh(999), 0, b"x").err(), Some(Errno::EBADF));
    assert_eq!(mount.decide_read(Fh(999), 0, 1).err(), Some(Errno::EBADF));
    let directory = mount.ask(|requests| requests.answer_opendir(ROOT)).unwrap();
    assert_eq!(mount.decide_read(directory, 0, 1).err(), Some(Errno::EBADF));
    assert_eq!(mount.ask(move |requests| requests.answer_fsync(reader)), Ok(()), "a handle that wrote nothing has nothing to sync");
    assert_eq!(mount.fixture.read_disk_file("a.txt").as_deref(), Some(&b"abc"[..]));
}

#[test]
fn the_bridges_own_lookup_is_answered_from_the_announcement_in_progress() {
    let mount = Requests::new();
    let kept = mount.fixture.make_own_file("kept.txt", b"kept").local_item_id;
    let gone = mount.fixture.make_own_file("gone.txt", b"gone").local_item_id;
    mount.remove("gone.txt", ItemKind::File).unwrap();
    let place_of = |name: &str| ItemPlace { parent_local_item_id: ROOT, name: name.into() };
    let find = |in_progress: Option<ItemAnnouncement>, name: &'static str| mount.ask(move |requests| requests.find_lookup_answer(in_progress.as_ref(), ROOT, OsStr::new(name)));

    // The name being announced as gone: the item that had it, of its kind, not to be kept —
    // though no row shows it there.
    let vanishing = ItemAnnouncement::Vanished { item: gone, former_place: place_of("gone.txt"), kind: ItemKind::File };
    let answer = find(Some(vanishing.clone()), "gone.txt").unwrap();
    assert!(matches!(answer, LookupAnswer::Vanishing(_)));
    assert_eq!((answer.attributes().ino, answer.attributes().kind, answer.lifetime()), (to_node_id(gone), FileType::RegularFile, Duration::ZERO));
    assert_eq!(find(None, "gone.txt").err(), Some(Errno::ENOENT), "anyone else is answered from the rows");

    // The name being announced as new is not there yet, though its row is.
    let appearing = ItemAnnouncement::Appeared { item: kept, place: place_of("kept.txt") };
    assert_eq!(find(Some(appearing), "kept.txt").err(), Some(Errno::ENOENT));

    // Any other name, and a file whose content changed: from the rows, as for everyone.
    assert!(matches!(find(Some(vanishing), "kept.txt"), Ok(LookupAnswer::Shown(entry)) if entry.item.local_item_id == kept));
    let changed = ItemAnnouncement::ContentChanged { item: kept, place: place_of("kept.txt") };
    assert!(matches!(find(Some(changed), "kept.txt"), Ok(LookupAnswer::Shown(entry)) if entry.attributes.size == 4));
}

/// The kernel's id of the started thread of `bridge`.
fn find_thread_id(bridge: &ItemInotifyBridge) -> u32 {
    let threads = std::fs::read_dir("/proc/self/task").unwrap().filter_map(|thread| thread.unwrap().file_name().to_str()?.parse::<u32>().ok());
    threads.into_iter().find(|thread_id| bridge.is_own_thread(*thread_id)).expect("the bridge's thread runs")
}

#[test]
fn the_bridges_own_requests_are_answered_from_the_rows_and_make_nothing() {
    let fixture = ItemFixture::new();
    // Over a directory of its own: what it would announce is not made in the folder.
    let announced_in = tempfile::tempdir().unwrap();
    let bridge = Arc::new(ItemInotifyBridge::new(announced_in.path().to_owned()));
    bridge.start(Arc::clone(&fixture.read_connections), None).unwrap();
    let mount = Requests::over(fixture, Arc::clone(&bridge));
    let its_thread = find_thread_id(&bridge);
    let kept = mount.fixture.make_own_file("kept.txt", b"kept").local_item_id;
    let rows_before = mount.fixture.list_items();
    // In a read-only folder too: the bridge announces there as well.
    mount.fixture.access_mode_switch.switch_to_read_only();
    let held = mount.fixture.tree_lock.try_lock().unwrap();

    // mkdir, mknod: the entry the rows have, nothing made.
    let answered = mount.make_as("kept.txt", MadeKind::File, its_thread).unwrap();
    assert_eq!((answered.entry.item.local_item_id, answered.fh), (kept, Fh::NOT_OPENED));
    assert_eq!(mount.make_as("none", MadeKind::Directory, its_thread).err(), Some(Errno::ENOENT));
    // unlink, rmdir: done for a name the folder does not show, and nothing removed of one it shows.
    assert_eq!(mount.remove_as("none.txt", ItemKind::File, its_thread), Ok(()));
    assert_eq!(mount.remove_as("kept.txt", ItemKind::File, its_thread), Err(Errno::EROFS));
    // setattr: as getattr.
    let touched = mount.set_time_as(kept, None, 5, its_thread).unwrap();
    assert_eq!((touched.ino, touched.size), (to_node_id(kept), 4));
    assert_ne!(seconds_of(touched.mtime), 5);
    // lookup, with nothing being announced: from the rows.
    let name = OsString::from("kept.txt");
    assert!(matches!(mount.ask(move |requests| requests.answer_lookup(ROOT, &name, its_thread)), Ok(LookupAnswer::Shown(entry)) if entry.item.local_item_id == kept));

    assert_eq!(mount.fixture.write_operation_thread.count_queued_jobs(), 0, "none of it is a job of the write-operation thread");
    drop(held);
    assert_eq!(mount.fixture.list_items(), rows_before);
    assert_eq!(mount.fixture.read_disk_file("kept.txt").as_deref(), Some(&b"kept"[..]));
    assert!(!mount.fixture.disk_path("none").exists());

    // The same requests of a program are refused: the folder is read-only.
    assert_eq!(mount.make("none", MadeKind::Directory).err(), Some(Errno::EROFS));
    bridge.stop();
}

/// What `mkdir -p`, `rm -r`, `mv` and a save by rename go on by: a row the store refuses
/// because the rows changed between the operation's read and its row reaches the program
/// as the errno of the rule, and the disk is as it was.
#[test]
fn a_row_the_store_refuses_reaches_the_reply_with_the_errno_of_its_rule() {
    use konedrive_tree::LocalVersionChange;
    let apply = |mount: &Requests, change: LocalVersionChange| {
        let store = mount.fixture.store.clone();
        mount.write_operations.run_before_row(move || {
            let change = change.clone();
            store.call_blocking(move |store| store.item_store().apply(&change).map(drop)).unwrap();
        });
    };
    let made_at = |directory: LocalItemId, name: &str| LocalVersionChange::Made { kind: ItemKind::Directory, place: ItemPlace { parent_local_item_id: directory, name: name.into() }, content: None, is_name_ignored: false, replaced: None };

    // mkdir of a name another program made meanwhile.
    let mount = Requests::new();
    apply(&mount, made_at(ROOT, "made"));
    assert_eq!(mount.make("made", MadeKind::Directory).err(), Some(Errno::EEXIST));
    assert!(!mount.fixture.disk_path("made").exists(), "what was made on disk is removed again");

    // rmdir of a directory something was made in meanwhile.
    let mount = Requests::new();
    let held = mount.fixture.make_own_directory("held").local_item_id;
    apply(&mount, made_at(held, "inside"));
    assert_eq!(mount.remove("held", ItemKind::Directory).err(), Some(Errno::ENOTEMPTY));
    assert!(mount.fixture.disk_path("held").is_dir(), "the directory is put back");

    // rename of a file that was removed meanwhile.
    let mount = Requests::new();
    let gone = mount.fixture.make_own_file("gone.txt", b"gone").local_item_id;
    apply(&mount, LocalVersionChange::Removed { item: gone });
    assert_eq!(mount.rename("gone.txt", "renamed.txt").err(), Some(Errno::ESTALE));
    assert!(!mount.fixture.disk_path("renamed.txt").exists(), "the entry is moved back");

    // mkdir in a directory that was removed meanwhile.
    let mount = Requests::new();
    let removed = mount.fixture.make_own_directory("removed").local_item_id;
    apply(&mount, LocalVersionChange::Removed { item: removed });
    assert_eq!(mount.make("removed/new", MadeKind::Directory).err(), Some(Errno::ENOENT));
    assert!(!mount.fixture.disk_path("removed/new").exists());

    assert_eq!(mount.fixture.write_rows(|store| store.item_store().list_place_change_records()), [], "the record went with the undo");
}

#[test]
fn a_part_of_a_directory_reading_hands_out_the_kept_listing_and_copies_nothing() {
    let mount = Requests::new();
    for name in ["a", "b", "c"] {
        mount.fixture.show_onedrive_file(name, b"");
    }
    let fh = mount.ask(|requests| requests.answer_opendir(ROOT)).unwrap();
    let first = mount.ask(move |requests| requests.answer_readdir(ROOT, fh, 0)).unwrap();
    assert_eq!(first.iter().map(local_name).collect::<Vec<_>>(), ["a", "b", "c"]);
    // Each later part is the listing that was kept, whole: the switch skips to its offset.
    for offset in [1, 2, 3] {
        let part = mount.ask(move |requests| requests.answer_readdir(ROOT, fh, offset)).unwrap();
        assert!(Arc::ptr_eq(&part, &first), "the part at {offset} is the kept listing itself");
    }
    assert!(Arc::ptr_eq(&mount.fixture.item_handles.find_listed_items(fh).unwrap(), &first));
    // A reading from the start lists anew.
    assert!(!Arc::ptr_eq(&mount.ask(move |requests| requests.answer_readdir(ROOT, fh, 0)).unwrap(), &first));
}

/// A content source for a download whose item OneDrive has another content of by now:
/// a whole fetch is answered with `size` bytes of `b'b'` under a version of their own,
/// `served` of which come before the stream waits for ever.
struct AnotherContentSource {
    size: u64,
    served: usize,
}

/// A stream that gives `left` bytes of `b'b'` and then waits for ever.
struct HeldStream {
    left: usize,
}

impl tokio::io::AsyncRead for HeldStream {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let given = self.left.min(buf.remaining());
        if given == 0 {
            return std::task::Poll::Pending;
        }
        buf.put_slice(&vec![b'b'; given]);
        self.left -= given;
        std::task::Poll::Ready(Ok(()))
    }
}

#[async_trait::async_trait]
impl crate::hydration::source::ContentSource for AnotherContentSource {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<crate::hydration::source::Fetched, crate::hydration::source::SourceError> {
        let version = crate::hydration::source::Version { ctag: "c-new".into(), quick_xor: Some([7; konedrive_graph::quickxor::LEN]) };
        Ok(crate::hydration::source::Fetched { served_from: from, size: self.size, mtime: std::time::UNIX_EPOCH, version: Some(version), stream: Box::new(HeldStream { left: self.served }) })
    }
}

/// The review's sequence: a handle opened as one content, OneDrive with another content
/// of the same size by now, which no listing said, and the download a read started.
#[test]
fn a_read_through_a_handle_is_never_answered_from_a_download_of_another_content() {
    const SIZE: usize = 3 << 20;
    const SERVED: usize = SIZE - (1 << 19);
    let fixture = ItemFixture::new();
    let bridge = Arc::clone(&fixture.inotify_bridge);
    let mount = Requests::downloading_from(fixture, bridge, Arc::new(AnotherContentSource { size: SIZE as u64, served: SERVED }));
    let row = mount.fixture.show_onedrive_file("big.bin", &vec![b'a'; SIZE]);
    let file = row.local_item_id;
    let fh = mount.open_to_read(file).unwrap().fh;

    // The program reads the first MiB as ranges, which reaches the threshold: the whole
    // file is downloaded, and what OneDrive answers that download is the other content.
    assert!(mount.read(fh, 0, DOWNLOAD_AFTER as u32).unwrap().iter().all(|byte| *byte == b'a'));
    let key = DownloadKey { item: file, content: local_content(&row) };
    let deadline = Instant::now() + Duration::from_secs(20);
    // Until the download ended, or wrote all the other content it is given.
    while mount.fixture.download_states.is_downloading(file) && mount.fixture.download_states.find_running_download(&key).is_none_or(|running| running.written_up_to < SERVED as u64) {
        assert!(Instant::now() < deadline, "the download neither ended nor wrote what it was given");
        std::thread::sleep(Duration::from_millis(2));
    }

    let read = |offset: u64| match mount.decide_read(fh, offset, 16) {
        Ok(ReadAnswer::Bytes(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(ReadAnswer::Range(range)) => Ok(format!("a range from {}", range.from)),
        Err(errno) => Err(errno),
    };
    assert_eq!(read(0), Err(Errno::ESTALE), "no byte of the other content is answered through the handle");
    assert_eq!(read(1 << 20), Err(Errno::ESTALE));
    assert!(!mount.fixture.download_states.is_downloading(file), "the download ended at its first answer");
    assert_eq!(mount.fixture.read_disk_file("big.bin"), None, "nothing is written under the item's name");
    assert_eq!(mount.row(file).content_presence, ContentPresence::NotOnDisk);
    assert_eq!(mount.row(file).local, row.local, "the row is as it was");
}

#[test]
fn a_read_past_the_threshold_starts_no_download_again_while_downloads_end_without_landing() {
    let mount = Requests::new();
    let content = vec![b'a'; 2 << 20];
    let file = mount.fixture.show_onedrive_file("big.bin", &content).local_item_id;
    let fh = mount.open_to_read(file).unwrap().fh;
    // Its place is held on disk by what no row says: every download ends at its landing.
    mount.fixture.write_disk_file("big.bin", b"nobody's");
    let count_whole_fetches = || mount.list_fetched_ranges().into_iter().filter(|fetch| fetch.1.is_none()).count();

    assert_eq!(mount.read(fh, 0, DOWNLOAD_AFTER as u32).unwrap().len(), DOWNLOAD_AFTER as usize);
    mount.wait_until_not_downloading(file);
    assert_eq!(count_whole_fetches(), 1, "the threshold starts the download");
    for read in 0..5u64 {
        assert_eq!(mount.read(fh, DOWNLOAD_AFTER + read * 4096, 4096).unwrap().len(), 4096);
        mount.wait_until_not_downloading(file);
    }
    assert_eq!(count_whole_fetches(), 1, "the reads that follow start none");
    assert_eq!(mount.fixture.read_disk_file("big.bin").as_deref(), Some(&b"nobody's"[..]));
}

#[test]
fn a_reader_whose_content_landed_reads_the_backing_file_beside_a_writer_that_opened_since() {
    let mount = Requests::new();
    let file = mount.fixture.show_onedrive_file("a.txt", b"0123456789").local_item_id;
    let reader = mount.open_to_read(file).unwrap().fh;
    assert!(matches!(mount.fixture.item_handles.find_file(reader), Some(OpenItemFile::ReadingNotOnDisk { .. })));

    // A program opens the file for writing: it is downloaded, and its row's content is
    // the writer's from here on, with the content the reader opened kept from before.
    let writer = mount.open_to_write(file).unwrap().fh;
    assert!(mount.row(file).content_before_write.is_some());

    assert!(matches!(mount.decide_read(reader, 2, 4), Ok(ReadAnswer::Bytes(bytes)) if bytes == b"2345"), "{:?}", mount.decide_read(reader, 2, 4));
    assert!(matches!(mount.fixture.item_handles.find_file(reader), Some(OpenItemFile::ReadingBackingFile { item, .. }) if item == file), "the handle goes on from the backing file");
    mount.release(writer);
}
