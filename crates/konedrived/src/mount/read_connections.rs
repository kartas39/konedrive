//! The mount's connections to the store for reading: one per thread of the mount.
//!
//! **State owned:** the connections, each opened for reading alone the first time its
//! thread asks, and the store's path. Nothing else opens or closes them; they go when the
//! mount stops, and no thread is given one after that.
//!
//! **Answers for:** a connection to the store per thread of the mount
//! (`docs/design/mount.md` §2.2). A read closure runs in ONE read transaction
//! (`TreeStore::read_items`): a row and its path, or a directory and its entries, are of
//! one moment. A lookup never waits for a listing's page or a step's commit.
//!
//! **Who reads through it:** the mount's own threads (fuser's) and the inotify bridge's
//! thread, and nobody else: a connection is kept per thread and never closed before the
//! mount stops, so a thread of a pool must not come here. A request that goes on on the
//! runtime (a read as ranges, an open for writing after a download) reads its rows on the
//! mount's thread before it leaves it. The write-operation thread reads through the
//! writer (`Store::call_blocking` and `TreeStore::read_items`): before it decides, it must
//! see the row it wrote itself a moment ago.
//!
//! **Tested without the rest:**
// test: each_thread_reads_through_a_connection_of_its_own
// test: a_thread_asks_twice_through_one_connection
// test: a_reader_sees_what_the_writer_committed_before_the_question
// test: a_closure_reads_one_moment_though_the_writer_commits_while_it_runs
// test: a_store_that_cannot_be_opened_for_reading_is_an_error_of_the_question_not_a_panic
// test: after_the_close_a_question_is_an_error_and_no_connection_is_opened

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use konedrive_tree::{ItemReader, TreeError, TreeStore};

/// One thread's connection. The lock is taken by that thread alone, for its own read: it
/// is what lets the map hold a connection that is not the map's thread's.
type ReadConnection = Arc<Mutex<TreeStore>>;

/// The reading connections of one mount.
pub(super) struct ReadConnections {
    store_path: PathBuf,
    /// The connections by the thread each was opened for; `None` once they are closed. A
    /// leaf mutex: held to find or to keep a connection, never while one is opened or read.
    by_thread: Mutex<Option<HashMap<ThreadId, ReadConnection>>>,
}

fn closed() -> TreeError {
    TreeError::Io(std::io::Error::other("the mount's reading connections are closed: the mount stopped"))
}

impl ReadConnections {
    /// The connections of the store at `store_path`; none is opened yet. Called by the
    /// mount's build, after the check at start and before the mount goes over the folder.
    pub(super) fn new(store_path: PathBuf) -> Self {
        ReadConnections { store_path, by_thread: Mutex::new(Some(HashMap::new())) }
    }

    /// Runs `read` over the rows on the calling thread's connection, in one read
    /// transaction, opening the connection with `TreeStore::open_read_only` at the
    /// thread's first question. The lock of the map is held only to find the thread's
    /// connection, never while it is opened and never while `read` runs. A store that
    /// cannot be opened is the question's error, and the next question tries again; so
    /// is a question after [`close`](Self::close).
    pub(super) fn read<T>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let connection = self.find_or_open_connection()?;
        let store = crate::panic::lock(&connection);
        store.read_items(read)
    }

    /// The calling thread's connection, opened when the thread has none yet.
    fn find_or_open_connection(&self) -> Result<ReadConnection, TreeError> {
        let thread = std::thread::current().id();
        match crate::panic::lock(&self.by_thread).as_ref() {
            None => return Err(closed()),
            Some(by_thread) => {
                if let Some(connection) = by_thread.get(&thread) {
                    return Ok(Arc::clone(connection));
                }
            }
        }
        // Opened with the map let go: no other thread asks for this thread's connection.
        let connection: ReadConnection = Arc::new(Mutex::new(TreeStore::open_read_only(&self.store_path)?));
        match crate::panic::lock(&self.by_thread).as_mut() {
            // Closed while this one was opened: it is dropped, and with it closed.
            None => Err(closed()),
            Some(by_thread) => {
                by_thread.insert(thread, Arc::clone(&connection));
                Ok(connection)
            }
        }
    }

    /// Closes every connection: the mount stops. Called by the mount's stop after the
    /// session ended, so that the store's files can be removed by a Forget. A read that
    /// runs on another thread meanwhile ends on its connection, which is closed with it;
    /// every question after this is an error.
    pub(super) fn close(&self) {
        let closed = crate::panic::lock(&self.by_thread).take();
        drop(closed);
    }

    /// How many connections are open.
    #[cfg(test)]
    pub(super) fn count_connections(&self) -> usize {
        crate::panic::lock(&self.by_thread).as_ref().map_or(0, HashMap::len)
    }
}

#[cfg(test)]
mod tests;
