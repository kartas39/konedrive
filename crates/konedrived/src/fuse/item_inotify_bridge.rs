//! The inotify bridge by item: what tells whoever watches the folder of a change a step
//! or a landing made, through the mount. What `inotify_bridge` is today over the node
//! table.
//!
//! **State owned:** the queue of announcements, the one being announced now, and the
//! bridge's thread. Queued by the steps towards the folder and the downloader
//! ([`ItemInotifyBridge::announce`]); performed by the bridge's own thread alone, which
//! creates, touches or removes the name BY PATH through the mount, so that the kernel
//! tells its watchers; the request that causes comes back to the mount on that thread and
//! is answered from [`ItemInotifyBridge::find_in_progress`] and the rows.
//!
//! **Answers for:** design §9, last paragraph. The former place travels in the
//! announcement (`ItemAnnouncement::Vanished`); the path of a place is found by the
//! bridge's thread through the rows (`ItemReader::find_path` of the directory, on a
//! reading connection of its own) when it performs the announcement.
// OPEN: what replaces the node table's "nobody watches a directory the kernel was never
// told of" (the map's E13): with no table every announcement makes the kernel look up
// each directory above it, and a first listing announces one name per item. Written as
// announcing everything; a filter is a measurement first.
//!
//! **Tested without the rest**: the queue by itself (the last three); the thread with the
//! mount replaced by a directory the test watches (the first):
// test: an_appearing_is_a_name_created_and_a_vanishing_a_name_removed_by_path
// test: the_announcement_being_performed_is_what_the_bridges_own_lookup_is_answered_from
// test: announcements_are_performed_in_the_order_they_were_queued
// test: a_stop_drops_what_is_queued_and_ends_the_thread
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::sync::Arc;

use super::item_announcements::ItemAnnouncement;
use super::read_connections::ReadConnections;

/// The bridge of one mount.
pub(super) struct ItemInotifyBridge {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    /// Given at the thread's start: the queue needs none.
    read_connections: std::sync::OnceLock<Arc<ReadConnections>>,
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
        ItemInotifyBridge { folder, read_connections: std::sync::OnceLock::new(), queued: std::sync::Mutex::default(), in_progress: std::sync::Mutex::new(None), is_queued: std::sync::Condvar::new(), thread_id: std::sync::OnceLock::new(), is_stopping: std::sync::atomic::AtomicBool::new(false), thread: std::sync::Mutex::new(None) }
    }

    /// Starts the bridge's thread, which reads the paths of what it announces through a
    /// reading connection of its own. Called by the mount once it is over the folder
    /// (`ItemMount::serve`).
    pub(super) fn start(self: &Arc<Self>, read_connections: Arc<ReadConnections>) -> std::io::Result<()> {
        todo!()
    }

    /// Queues the announcements of one change of the folder, to be performed together and
    /// in order. Called after the change's row was committed, never before: the bridge's
    /// requests are answered from the rows.
    pub(super) fn announce(&self, announcements: Vec<ItemAnnouncement>) {
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
