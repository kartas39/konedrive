//! The mount's connections to the store for reading: one per thread of the mount.
//!
//! **State owned:** the connections, each opened for reading alone the first time its
//! thread asks, and the store's path. Nothing else opens or closes them; they go when the
//! mount stops. Today one connection behind one lock serves the four threads
//! (`directory_contents.rs:47`).
//!
//! **Answers for:** design §9, "a connection to the store per thread of the mount"; the
//! map's MISSING-1.q on the mount's side. A read closure runs in ONE read transaction
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
#![allow(dead_code, unused_variables)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use konedrive_tree::{ItemReader, TreeError, TreeStore};

/// The reading connections of one mount.
pub(super) struct ReadConnections {
    store_path: PathBuf,
    by_thread: Mutex<HashMap<ThreadId, Arc<Mutex<TreeStore>>>>,
}

impl ReadConnections {
    /// The connections of the store at `store_path`; none is opened yet. Called by the
    /// mount's build, after the check at start and before the mount goes over the folder.
    pub(super) fn new(store_path: PathBuf) -> Self {
        todo!()
    }

    /// Runs `read` over the rows on the calling thread's connection, in one read
    /// transaction, opening the connection with `TreeStore::open_read_only` at the
    /// thread's first question. The lock of the map is held only to find the thread's
    /// connection, never while `read` runs.
    pub(super) fn read<T>(&self, read: impl FnOnce(&ItemReader<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        todo!()
    }

    /// Closes every connection: the mount stops. Called by the mount's stop after the
    /// session ended, so that the store's files can be removed by a Forget.
    pub(super) fn close(&self) {
        todo!()
    }
}
