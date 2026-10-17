//! The inotify bridge by item: what tells whoever watches the folder of a change a step
//! or a landing made, through the mount (`docs/design/mount.md` §9).
//!
//! **State owned:** the queue of announcements, the one being announced now, and the
//! bridge's thread. Queued by the steps towards the folder and the downloader
//! ([`ItemInotifyBridge::announce`]); performed by the bridge's own thread alone, which
//! creates, touches or removes the name BY PATH through the mount, so that the kernel
//! tells its watchers; the request that causes comes back to the mount on that thread and
//! is answered from [`ItemInotifyBridge::find_in_progress`] and the rows.
//!
//! **Answers for:** what a step, a landing, a free-up and a pin changed behind the
//! kernel's back, told to its watchers. The former place travels in the
//! announcement (`ItemAnnouncement::Vanished`); the path of a place is found by the
//! bridge's thread through the rows (`ItemReader::find_path` of the directory, on a
//! reading connection of its own) when it performs the announcement.
// Every announcement is performed, whether anybody watches its directory or not: the
// kernel looks up each directory above it, and a first listing announces one name per
// item.
//!
//! **Tested without the rest**: the queue by itself (the last three); the thread with the
//! mount replaced by a directory the test watches (the first):
// test: an_appearing_is_a_name_created_and_a_vanishing_a_name_removed_by_path
// test: the_announcement_being_performed_is_what_the_bridges_own_lookup_is_answered_from
// test: announcements_are_performed_in_the_order_they_were_queued
// test: a_stop_drops_what_is_queued_and_ends_the_thread
// test: the_thread_is_started_once_told_by_its_id_and_ended_by_the_stop
// test: an_announcement_is_performed_once
// test: a_changed_content_is_a_touch_of_the_file_by_path
// test: the_kernels_pages_of_a_changed_content_are_dropped_before_the_file_is_touched
// test: an_announcement_in_a_directory_the_folder_no_longer_shows_is_skipped_and_the_next_performed

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::{LocalItemId, TreeError};

use super::backing_directory::{to_mode_bits, EntryKind, WriteBits};
use super::item_announcements::{decide_system_call, AnnouncementSystemCall, ItemAnnouncement};
use super::item_attributes::to_node_id;
use super::read_connections::ReadConnections;

/// The kernel's cached pages of the mount's files: what drops those of one file. The
/// session's notifier is one (`FUSE_NOTIFY_INVAL_INODE`); a test gives a stand-in.
pub(super) trait KernelPageCache: Send + Sync {
    /// Drops every page the kernel cached of the file `node`, and its cached attributes:
    /// the next read asks the mount.
    fn invalidate(&self, node: LocalItemId) -> std::io::Result<()>;
}

impl KernelPageCache for fuser::Notifier {
    fn invalidate(&self, node: LocalItemId) -> std::io::Result<()> {
        // From the first byte to the last.
        self.inval_inode(to_node_id(node), 0, 0)
    }
}

/// The bridge of one mount.
pub(crate) struct ItemInotifyBridge {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    /// Given at the thread's start: the queue needs none.
    read_connections: std::sync::OnceLock<Arc<ReadConnections>>,
    /// Given at the thread's start, when there is a mount whose kernel caches pages.
    page_cache: std::sync::OnceLock<Arc<dyn KernelPageCache>>,
    queued: std::sync::Mutex<std::collections::VecDeque<Vec<ItemAnnouncement>>>,
    in_progress: std::sync::Mutex<Option<ItemAnnouncement>>,
    is_queued: std::sync::Condvar,
    thread_id: std::sync::OnceLock<u32>,
    is_stopping: std::sync::atomic::AtomicBool,
    /// The bridge's thread, kept by its start for the stop to wait for.
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ItemInotifyBridge {
    /// The bridge's queue, with no thread: made by the mount's build. The queue (`new`,
    /// `announce`, `find_in_progress`, `is_own_thread`, `stop`) is filled first, with the
    /// daemon's leaves, since every runner of steps and the downloader announce through
    /// it; the thread (`start`, and the performing by path) with the reading requests.
    pub(super) fn new(folder: PathBuf) -> Self {
        ItemInotifyBridge { folder, read_connections: std::sync::OnceLock::new(), page_cache: std::sync::OnceLock::new(), queued: std::sync::Mutex::default(), in_progress: std::sync::Mutex::new(None), is_queued: std::sync::Condvar::new(), thread_id: std::sync::OnceLock::new(), is_stopping: std::sync::atomic::AtomicBool::new(false), thread: std::sync::Mutex::new(None) }
    }

    /// Starts the bridge's thread, which reads the paths of what it announces through a
    /// reading connection of its own. Called by the mount once it is over the folder
    /// (`ItemMount::serve`).
    ///
    /// When this returns the thread runs and its id is known
    /// ([`is_own_thread`](Self::is_own_thread)): no request of it can come before. A
    /// second start starts nothing, nor does a start after the stop.
    ///
    /// `page_cache` is the kernel's cached pages of this mount — the session's notifier
    /// (`fuser::Notifier` is one) — and `None` where no mount is over the folder: before
    /// the file of a changed content is touched, the pages the kernel cached of it are
    /// dropped through it, so that no read through the mount is answered the old content
    /// from the kernel's cache.
    pub(super) fn start(self: &Arc<Self>, read_connections: Arc<ReadConnections>, page_cache: Option<Arc<dyn KernelPageCache>>) -> std::io::Result<()> {
        // Held from the look to the keeping of the handle: one start at a time. The stop
        // takes this lock only after its own flag is set, and the thread never takes it.
        let mut thread = crate::panic::lock(&self.thread);
        if thread.is_some() || self.is_stopping.load(std::sync::atomic::Ordering::SeqCst) || self.read_connections.set(read_connections).is_err() {
            return Ok(());
        }
        if let Some(page_cache) = page_cache {
            let _ = self.page_cache.set(page_cache);
        }
        let (bridge, (id_sender, id)) = (Arc::clone(self), std::sync::mpsc::channel());
        let started = std::thread::Builder::new().name("konedrive-announce".into()).spawn(move || {
            // SAFETY: a system call that takes nothing and cannot fail.
            let _ = id_sender.send(unsafe { libc::gettid() } as u32);
            bridge.perform_queued();
        })?;
        if let Ok(id) = id.recv() {
            let _ = self.thread_id.set(id);
        }
        *thread = Some(started);
        Ok(())
    }

    /// The thread: performs what is queued, one change after the other, until the stop.
    /// The kernel's cached pages of a file whose content changed are dropped first
    /// ([`KernelPageCache::invalidate`]), where the start was given them. Then each
    /// announcement is one system call by path through the mount
    /// (`item_announcements::decide_system_call`), which the mount answers on this
    /// thread's behalf from [`find_in_progress`](Self::find_in_progress) and the rows,
    /// making nothing: so the kernel tells whoever watches the directory. After a stop
    /// the announcement being performed is finished and no other is begun.
    fn perform_queued(&self) {
        while let Some(announcements) = self.wait_for_announcements() {
            for announcement in announcements {
                if self.is_stopping.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let system_call = match self.find_system_call(&announcement) {
                    Ok(Some(system_call)) => system_call,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!("a change of item {} is not announced to the kernel: its place could not be read: {error}", announcement.item());
                        continue;
                    }
                };
                // The kernel's cached pages of the old content go first: the touch tells
                // the watchers, and whoever reads then asks the mount. A change of the
                // marks alone is a touch too, and drops nothing: the content is as it was.
                if let (ItemAnnouncement::ContentChanged { .. }, AnnouncementSystemCall::Utimensat(_), Some(page_cache)) = (&announcement, &system_call, self.page_cache.get()) {
                    if let Err(error) = page_cache.invalidate(announcement.item()) {
                        tracing::debug!("the kernel's cached pages of item {} could not be dropped: {error}", announcement.item());
                    }
                }
                self.mark_in_progress(Some(announcement));
                if let Err(error) = self.make_system_call(&system_call) {
                    tracing::debug!("{system_call:?} through the mount, for an announcement: {error}");
                }
                self.mark_in_progress(None);
            }
        }
    }

    /// The system call of `announcement`, from what the rows say now: the path of the
    /// directory its place is in and the kind of its item, in one read closure of this
    /// thread's own reading connection.
    fn find_system_call(&self, announcement: &ItemAnnouncement) -> Result<Option<AnnouncementSystemCall>, TreeError> {
        let Some(read_connections) = self.read_connections.get() else { return Ok(None) };
        let (directory_path, kind_now) = read_connections.read(|reader| {
            let directory_path = reader.find_path(announcement.place().parent_local_item_id)?;
            let kind_now = reader.find_item(announcement.item())?.filter(|item| item.local.is_some()).map(|item| item.kind);
            Ok((directory_path, kind_now))
        })?;
        Ok(decide_system_call(announcement, directory_path, kind_now))
    }

    /// Makes `system_call` at its path in the folder, which is the mount.
    fn make_system_call(&self, system_call: &AnnouncementSystemCall) -> std::io::Result<()> {
        let to_c_path = |path: &Path| CString::new(self.folder.join(path).as_os_str().as_bytes()).map_err(std::io::Error::other);
        // SAFETY: plain system calls on a path this function owns.
        let answer = unsafe {
            match system_call {
                AnnouncementSystemCall::Mkdir(path) => libc::mkdir(to_c_path(path)?.as_ptr(), to_mode_bits(EntryKind::Directory, WriteBits::Present)),
                AnnouncementSystemCall::Mknod(path) => libc::mknod(to_c_path(path)?.as_ptr(), libc::S_IFREG | to_mode_bits(EntryKind::File, WriteBits::Present), 0),
                AnnouncementSystemCall::Rmdir(path) => libc::rmdir(to_c_path(path)?.as_ptr()),
                AnnouncementSystemCall::Unlink(path) => libc::unlink(to_c_path(path)?.as_ptr()),
                AnnouncementSystemCall::Utimensat(path) => libc::utimensat(libc::AT_FDCWD, to_c_path(path)?.as_ptr(), std::ptr::null(), 0),
            }
        };
        match answer {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// Queues the announcements of one change of the folder, to be performed together and
    /// in order. Called after the change's row was committed, never before: the bridge's
    /// requests are answered from the rows.
    pub(crate) fn announce(&self, announcements: Vec<ItemAnnouncement>) {
        if announcements.is_empty() {
            return;
        }
        let mut queued = crate::panic::lock(&self.queued);
        // Asked under the queue's lock, which the stop empties the queue under: nothing
        // is queued behind a stop.
        if self.is_stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        queued.push_back(announcements);
        self.is_queued.notify_one();
    }

    /// What is queued and not performed yet, a change after the other: for the tests of
    /// what announces — a step towards the folder, a download, a free-up — over a bridge
    /// whose thread was not started, which is the one way to see what they announced.
    #[cfg(test)]
    pub(crate) fn list_queued(&self) -> Vec<Vec<ItemAnnouncement>> {
        crate::panic::lock(&self.queued).iter().cloned().collect()
    }

    /// Whether the kernel's thread id `thread_id` is the bridge's own: its requests are
    /// answered as `item_announcements::find_announced_name` says.
    pub(super) fn is_own_thread(&self, thread_id: u32) -> bool {
        self.thread_id.get() == Some(&thread_id)
    }

    /// The announcement being performed now. Called by lookup, mkdir, mknod, unlink, rmdir
    /// and setattr when the request is the bridge's own.
    pub(super) fn find_in_progress(&self) -> Option<ItemAnnouncement> {
        crate::panic::lock(&self.in_progress).clone()
    }

    /// The announcements of the change that was queued first, taken off the queue; waits
    /// while nothing is queued. `None` once the bridge is stopping: the thread ends.
    /// Called by the bridge's thread alone, which performs them one by one.
    fn wait_for_announcements(&self) -> Option<Vec<ItemAnnouncement>> {
        let mut queued = crate::panic::lock(&self.queued);
        loop {
            if self.is_stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            if let Some(announcements) = queued.pop_front() {
                return Some(announcements);
            }
            queued = self.is_queued.wait(queued).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// `announcement` is being performed from now on, or nothing is. Called by the
    /// bridge's thread around each name it creates, touches or removes.
    fn mark_in_progress(&self, announcement: Option<ItemAnnouncement>) {
        *crate::panic::lock(&self.in_progress) = announcement;
    }

    /// Stops the thread; what is queued is dropped. Called by the mount's stop.
    ///
    /// It waits for the thread: the announcement being performed is finished, and when
    /// this returns nothing more is made by path through the mount — which, once the
    /// mount is off, would land in the backing directory. Never called on the bridge's
    /// own thread, nor by what a request of that thread waits for.
    pub(super) fn stop(&self) {
        {
            let mut queued = crate::panic::lock(&self.queued);
            self.is_stopping.store(true, std::sync::atomic::Ordering::SeqCst);
            queued.clear();
            // Under the queue's lock: the thread is either before its look at the flag or
            // in its wait.
            self.is_queued.notify_all();
        }
        // Taken out first: the lock is not held while the thread is waited for.
        let thread = crate::panic::lock(&self.thread).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
