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
//! **Tested without the rest**, with the mount replaced by a directory the test watches:
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
    read_connections: Arc<ReadConnections>,
    queued: std::sync::Mutex<std::collections::VecDeque<Vec<ItemAnnouncement>>>,
    in_progress: std::sync::Mutex<Option<ItemAnnouncement>>,
    is_queued: std::sync::Condvar,
    thread_id: std::sync::OnceLock<u32>,
    is_stopping: std::sync::atomic::AtomicBool,
}

impl ItemInotifyBridge {
    /// Made by the mount's build; its thread is started once the mount is over the folder.
    pub(super) fn new(folder: PathBuf, read_connections: Arc<ReadConnections>) -> Self {
        todo!()
    }

    /// Starts the bridge's thread. Called by the mount once it is over the folder.
    pub(super) fn start(self: &Arc<Self>) -> std::io::Result<()> {
        todo!()
    }

    /// Queues the announcements of one change of the folder, to be performed together and
    /// in order. Called after the change's row was committed, never before: the bridge's
    /// requests are answered from the rows.
    pub(super) fn announce(&self, announcements: Vec<ItemAnnouncement>) {
        todo!()
    }

    /// Whether the kernel's thread id `thread_id` is the bridge's own: its requests are
    /// answered as `item_announcements::find_announced_name` says.
    pub(super) fn is_own_thread(&self, thread_id: u32) -> bool {
        todo!()
    }

    /// The announcement being performed now. Called by lookup, mkdir, mknod, unlink, rmdir
    /// and setattr when the request is the bridge's own.
    pub(super) fn find_in_progress(&self) -> Option<ItemAnnouncement> {
        todo!()
    }

    /// Stops the thread; what is queued is dropped. Called by the mount's stop.
    pub(super) fn stop(&self) {
        todo!()
    }
}
