//! The store shared by the tasks of one folder: one thread owns each connection.

use std::path::PathBuf;

use super::{outbox, ReadStore, TreeError, TreeStore};

/// A job for a store's owner thread: a closure over the store, which sends
/// its own answer.
type Job = Box<dyn FnOnce(&mut TreeStore) + Send>;

/// Jobs a store's channel holds before a sender waits (issue #38).
pub const QUEUE: usize = 1024;

/// Hands out the ids of the owner threads.
static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// The id of the store this thread owns; 0 on any other thread.
    static OWNING: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One thread that owns a connection and runs the jobs sent to it, one at a
/// time, in the order they arrive (issue #38).
struct Owner {
    jobs: tokio::sync::mpsc::Sender<Job>,
    id: u64,
    /// True once the thread has dropped its connection ([`Store::close`]).
    closed: tokio::sync::watch::Receiver<bool>,
}

impl Owner {
    /// Starts the thread. It ends, dropping the connection, once every
    /// sender is gone and the jobs already queued have run. After each job
    /// that changed the outbox, `changes` tells those waiting for a change.
    fn spawn(mut store: TreeStore, name: &str, changes: Option<std::sync::Arc<outbox::OutboxChanges>>) -> Self {
        let id = NEXT_OWNER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (jobs, mut queue) = tokio::sync::mpsc::channel::<Job>(QUEUE);
        let (closing, closed) = tokio::sync::watch::channel(false);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                OWNING.with(|owning| owning.set(id));
                while let Some(job) = queue.blocking_recv() {
                    let before = changes.as_ref().map(|c| c.generation());
                    // A job that panics answers nobody (its caller gets an error);
                    // its transaction, if any, is rolled back as it is dropped.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut store))).is_err() {
                        tracing::error!("a job of the tree store panicked; the store goes on");
                    }
                    if let (Some(changes), Some(before)) = (&changes, before) {
                        if changes.generation() != before {
                            changes.committed();
                        }
                    }
                }
                // The connection is closed before anyone is told so.
                drop(store);
                let _ = closing.send(true);
            })
            .expect("the tree store's thread starts");
        Owner { jobs, id, closed }
    }

    /// A call from inside one of this owner's jobs would wait for itself:
    /// a bug, which panics in debug and test builds and is an error otherwise.
    fn refuse_reentry(&self) -> Result<(), TreeError> {
        if OWNING.with(|owning| owning.get()) != self.id {
            return Ok(());
        }
        debug_assert!(false, "a job of the tree store called the store: it would wait for itself");
        Err(TreeError::Io(std::io::Error::other("a job of the tree store called the store")))
    }

    fn job<T: Send + 'static>(
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> (Job, tokio::sync::oneshot::Receiver<Result<T, TreeError>>) {
        let (answer, answered) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |store| {
            let _ = answer.send(f(store));
        });
        (job, answered)
    }

    async fn call<T: Send + 'static>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.refuse_reentry()?;
        let (job, answered) = Self::job(f);
        self.jobs.send(job).await.map_err(|_| stopped())?;
        answered.await.map_err(|_| failed())?
    }

    fn call_blocking<T: Send + 'static>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.refuse_reentry()?;
        let (job, answered) = Self::job(f);
        self.jobs.blocking_send(job).map_err(|_| stopped())?;
        answered.blocking_recv().map_err(|_| failed())?
    }
}

fn stopped() -> TreeError {
    TreeError::Io(std::io::Error::other("the tree store's thread has stopped"))
}

fn failed() -> TreeError {
    TreeError::Io(std::io::Error::other("a job of the tree store failed"))
}

/// The store, shared by the tasks of one folder: the listing, the
/// materializer, the outbox worker and the D-Bus queries. One thread owns
/// its connection, and everyone else sends it jobs (issue #38): `call` from
/// async code, `call_blocking` from plain threads. Only that thread holds a
/// read-write connection to the store; the bus's reads go to a second,
/// read-only connection with a thread of its own.
#[derive(Clone)]
pub struct Store {
    owner: std::sync::Arc<Owner>,
    changes: std::sync::Arc<outbox::OutboxChanges>,
    /// The read-only connection's owner ([`Store::read`]), started when
    /// first used; `None` inside when it cannot be opened.
    reader: std::sync::Arc<std::sync::OnceLock<Option<Owner>>>,
    path: Option<PathBuf>,
    /// The pause as the store last had it: [`NOT_PAUSED`],
    /// or paused until then, 0 for until resumed. Kept here so that it is
    /// read without a job, from anywhere.
    pause: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

/// [`Store::pause`]'s memory while not paused.
const NOT_PAUSED: i64 = -1;

impl Store {
    pub fn new(store: TreeStore) -> Self {
        let changes = std::sync::Arc::clone(&store.changes);
        let path = store.path.clone();
        let pause = store.paused_until().ok().flatten().unwrap_or(NOT_PAUSED);
        let owner = Owner::spawn(store, "konedrive-store", Some(std::sync::Arc::clone(&changes)));
        Self {
            owner: std::sync::Arc::new(owner),
            changes,
            reader: Default::default(),
            path,
            pause: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(pause)),
        }
    }

    /// Lets go of this handle and waits until the store's connections are closed: that is,
    /// until every other clone has been dropped too and the owner threads have ended. For
    /// whoever removes the store's files, which must not go while a connection is open:
    /// SQLite's last close removes the journal beside the database by name, and would
    /// take a newer store's with it. The caller bounds the wait.
    pub fn close(self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut waits = vec![self.owner.closed.clone()];
        if let Some(Some(reader)) = self.reader.get() {
            waits.push(reader.closed.clone());
        }
        drop(self);
        async move {
            for mut closed in waits {
                // An owner thread that ended without saying so has closed too.
                let _ = closed.wait_for(|closed| *closed).await;
            }
        }
    }

    /// The pause as last written: paused until then (unix seconds, 0 for
    /// until resumed), or `None`. From memory: no job.
    pub fn pause(&self) -> Option<i64> {
        let until = self.pause.load(std::sync::atomic::Ordering::SeqCst);
        (until != NOT_PAUSED).then_some(until)
    }

    /// Writes the pause (`None`: resumed), and remembers it.
    pub async fn set_pause(&self, until: Option<i64>) -> Result<(), TreeError> {
        self.call(move |s| s.set_paused_until(until)).await?;
        self.pause.store(until.map_or(NOT_PAUSED, |u| u.max(0)), std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// [`set_pause`](Self::set_pause) for plain threads.
    pub fn set_pause_blocking(&self, until: Option<i64>) -> Result<(), TreeError> {
        self.call_blocking(move |s| s.set_paused_until(until))?;
        self.pause.store(until.map_or(NOT_PAUSED, |u| u.max(0)), std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// A timed pause until `until` has run out: forgotten here, and taken
    /// off `meta` by a job nobody waits for (unless a pause was written since).
    pub fn pause_ended(&self, until: i64) {
        use std::sync::atomic::Ordering;
        if self.pause.compare_exchange(until, NOT_PAUSED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let (job, _) = Owner::job(move |s| {
            if s.paused_until()? == Some(until) {
                s.set_paused_until(None)?;
            }
            Ok(())
        });
        let _ = self.owner.jobs.try_send(job);
    }

    /// Runs `f` on the store's thread and waits for its answer: for async
    /// code. Jobs run one at a time, in the order they arrive; `f` may hold a
    /// transaction, and must never await, block on anything but SQLite, or
    /// call the store.
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        self.owner.call(f).await
    }

    /// [`call`](Self::call) for plain threads (the examiner, the
    /// materializer, the body of a `spawn_blocking`): waits for the answer.
    /// On an async runtime's thread it panics (tokio refuses to block there).
    pub fn call_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        self.owner.call_blocking(f)
    }

    /// The read-only connection's owner, started when first asked for; `None`
    /// for a store in memory, or one that cannot be opened for reading alone.
    fn reader(&self) -> Option<&Owner> {
        let path = self.path.as_ref()?;
        self.reader
            .get_or_init(|| match TreeStore::open_read_only(path) {
                Ok(opened) => Some(Owner::spawn(opened, "konedrive-store-read", None)),
                Err(e) => {
                    tracing::warn!("the tree store cannot be opened for reading alone ({e}); it is read through its own thread");
                    None
                }
            })
            .as_ref()
    }

    /// Runs `f` on the store's read-only connection, which never waits for a
    /// writer and sees what was last committed (issue #38): the bus's lists and
    /// sums. A store in memory, or one whose second connection cannot be
    /// opened, is read through its own thread; `f` is handed the same
    /// [`ReadStore`] either way.
    pub async fn read<T: Send + 'static>(&self, f: impl FnOnce(&ReadStore<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        let f = move |store: &mut TreeStore| f(&ReadStore::of(store));
        match self.reader() {
            Some(reader) => reader.call(f).await,
            None => self.call(f).await,
        }
    }

    /// [`read`](Self::read) for plain threads.
    pub fn read_blocking<T: Send + 'static>(&self, f: impl FnOnce(&ReadStore<'_>) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        let f = move |store: &mut TreeStore| f(&ReadStore::of(store));
        match self.reader() {
            Some(reader) => reader.call_blocking(f),
            None => self.call_blocking(f),
        }
    }

    /// What changed in the outbox, shared with the store.
    pub fn changes(&self) -> &std::sync::Arc<outbox::OutboxChanges> {
        &self.changes
    }
}
