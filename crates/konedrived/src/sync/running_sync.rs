//! The running sync of a OneDrive folder, as one object in the folder's state
//! ([`RunningSync`]): it owns its parts — the poller, the watcher and the outbox worker of a
//! read-write folder, the thumbnail filler, the sign-in nudge. The tree store is the
//! folder's, kept with its record until a Forget; the sync holds clones of it.
//!
//! # Stopping is dropping
//!
//! A `RunningSync` that is dropped tells every part to stop, at once, and hands the parts
//! to the service's [`Ended`], because a drop cannot wait. Whoever changes the folder next
//! waits there for every part ([`SyncService::change`](super::SyncService::change)) before
//! it touches anything, so a section a part had begun has run to its end by then. This is
//! the only way a sync stops: `change` takes it out of the state, which drops it. A change
//! that is itself dropped part-way therefore leaves no half of a sync running: the state
//! says stopped, the parts are told, and the next change waits for them.

use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::local::watcher::{WatchHandle, Watcher};
use crate::remote::listing::{PollHandle, Poller};
use crate::upload::kept_back::SummaryRow;
use crate::upload::{OutboxHandle, OutboxWorker};

/// Whether the sync of a OneDrive folder that is up runs.
pub(super) enum Sync {
    Stopped(Why),
    Running(Box<RunningSync>),
}

/// Why a OneDrive folder's sync does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Why {
    /// Nothing has started it yet: the folder came up a moment ago, or is not intercepted.
    NotStarted,
    /// A change of the folder stopped it and has not started it again: the change is
    /// under way, or was cut before its end. The next change starts it.
    Interrupted,
    /// It could not start. The sentence; `Refresh()` tries again.
    CannotStart(String),
}

/// What a OneDrive folder's sync does about local changes.
pub(super) enum Writes {
    /// The folder is under the read-only lock: nothing watches it and nothing is uploaded.
    /// Whether a cycle runs is the poller's to say, at each one: a locked folder that
    /// still holds changes waiting to be uploaded runs none.
    Locked {
        /// Why a folder whose account is read-write runs locked all the same.
        note: Option<String>,
    },
    /// The folder is watched, and what its watcher records is uploaded. Neither part
    /// exists without the other.
    Open {
        watcher: Watcher,
        outbox: OutboxWorker,
        /// Shared with the task of the sync that says when the watcher's walk is over.
        lock: Arc<Mutex<Lock>>,
    },
}

/// The read-only lock of a folder whose watcher and worker run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Lock {
    /// Still on: the watcher is walking the folder, and the lock comes off when it has.
    Walking,
    /// Off: the folder can be changed, and what is changed is uploaded.
    Off,
    /// It stays on: the watcher did not finish walking the folder. The sentence.
    Stays(String),
}

/// What a sync does about local changes, without its parts: what `publish` shows of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Uploading {
    /// Locked, with why when the account is read-write.
    Locked(Option<String>),
    Open(Lock),
}

/// What a reader may reach of a running sync: what wakes its parts or tells them to stop. Nothing here can wait for a part; only the [`RunningSync`] owns them.
#[derive(Clone)]
pub(super) struct Handles {
    /// The number of this sync, among those of the daemon: which sync a part belonged to.
    pub id: u64,
    pub poll: PollHandle,
    /// The watcher and the outbox worker of a read-write folder.
    pub writes: Option<(WatchHandle, OutboxHandle)>,
    /// `NotUploadedSummary()` as the outbox worker last summed it (issue #38): answered
    /// from memory while the worker runs.
    pub kept_back: Arc<Mutex<Option<Vec<SummaryRow>>>>,
    /// Stops the tasks that have no handle of their own: the sign-in nudge and the
    /// thumbnail filler.
    pub(super) stop: CancellationToken,
    /// The tidying a cycle asked for (a `move-out` row dropped because its item is gone
    /// from OneDrive): tasks of this sync, waited for with it.
    pub(super) tidying: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Handles {
    /// Tells every part to stop, at once, without waiting for any.
    pub fn cancel(&self) {
        self.stop.cancel();
        self.poll.cancel();
        if let Some((watcher, outbox)) = &self.writes {
            outbox.cancel();
            watcher.cancel();
        }
    }

    /// Whether the sync was told to stop ([`cancel`](Self::cancel)): its parts end, or
    /// have ended.
    pub fn told_to_stop(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub fn outbox(&self) -> Option<&OutboxHandle> {
        self.writes.as_ref().map(|(_, outbox)| outbox)
    }

    pub fn watcher(&self) -> Option<&WatchHandle> {
        self.writes.as_ref().map(|(watcher, _)| watcher)
    }
}

/// A OneDrive folder's sync while it runs. See the module for how it stops.
pub(super) struct RunningSync {
    handles: Handles,
    /// `None` only while it is being dropped.
    parts: Option<Parts>,
    ended: Ended,
}

/// The parts a running sync owns.
pub(super) struct Parts {
    pub poller: Poller,
    /// Asks for a cycle when the account signs in.
    pub sign_in: JoinHandle<()>,
    /// The thumbnail filler; none without a cache to fill.
    pub thumbnails: Option<JoinHandle<()>>,
    pub writes: Writes,
}

impl RunningSync {
    pub fn new(handles: Handles, parts: Parts, ended: Ended) -> Self {
        Self { handles, parts: Some(parts), ended }
    }

    pub fn handles(&self) -> &Handles {
        &self.handles
    }

    pub fn writes(&self) -> Option<&Writes> {
        self.parts.as_ref().map(|parts| &parts.writes)
    }

    /// What the sync does about local changes.
    pub fn uploading(&self) -> Uploading {
        match self.writes() {
            Some(Writes::Open { lock, .. }) => Uploading::Open(crate::panic::lock(lock).clone()),
            Some(Writes::Locked { note }) => Uploading::Locked(note.clone()),
            None => Uploading::Locked(None),
        }
    }

    /// Where a read-write sync's lock is said, for whoever waits for the watcher's walk.
    pub fn lock_cell(&self) -> Option<Arc<Mutex<Lock>>> {
        match self.writes() {
            Some(Writes::Open { lock, .. }) => Some(Arc::clone(lock)),
            _ => None,
        }
    }
}

impl Drop for RunningSync {
    fn drop(&mut self) {
        self.handles.cancel();
        if let Some(parts) = self.parts.take() {
            let (watcher, outbox) = match parts.writes {
                Writes::Open { watcher, outbox, .. } => (Some(watcher), Some(outbox)),
                Writes::Locked { .. } => (None, None),
            };
            crate::panic::lock(&self.ended.0).push(Ending {
                poller: Some(parts.poller),
                outbox,
                watcher,
                watcher_stop: None,
                sign_in: Some(parts.sign_in),
                thumbnails: parts.thumbnails,
                tidying: Arc::clone(&self.handles.tidying),
                tidied: None,
            });
        }
    }
}

/// The parts of syncs that were told to stop and have not been waited for yet. The
/// service's, shared with every sync it starts.
#[derive(Clone, Default)]
pub(super) struct Ended(Arc<Mutex<Vec<Ending>>>);

/// The parts of one stopped sync, each kept until it has been waited for.
struct Ending {
    poller: Option<Poller>,
    outbox: Option<OutboxWorker>,
    watcher: Option<Watcher>,
    /// The blocking task that joins the watcher's threads.
    watcher_stop: Option<JoinHandle<()>>,
    sign_in: Option<JoinHandle<()>>,
    thumbnails: Option<JoinHandle<()>>,
    tidying: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The tidying task being waited for.
    tidied: Option<JoinHandle<()>>,
}

impl Ending {
    /// Waits for every part, in the order the locks ask for. A part that has been waited
    /// for is let go; cut while it waits, this goes on from that part at the next call.
    async fn join(&mut self) {
        // The poller first: a cycle may hold the tree lock, which the watcher's examination
        // and the worker's commit wait for.
        if let Some(poller) = self.poller.as_mut() {
            poller.join().await;
            self.poller = None;
        }
        // The outbox worker next: a request under way was cut off, and its row is replayed
        // when a worker starts again (`docs/design/writes.md` §10) — and a commit it holds
        // the tree lock for does not keep the examination below waiting.
        if let Some(outbox) = self.outbox.as_ref() {
            outbox.stop().await;
            self.outbox = None;
        }
        // The watcher: a blocking join, off the runtime; the examination under way
        // finishes first.
        if let Some(watcher) = self.watcher.take() {
            self.watcher_stop = Some(tokio::task::spawn_blocking(move || watcher.stop()));
        }
        if let Some(stop) = self.watcher_stop.as_mut() {
            if let Err(e) = stop.await {
                tracing::warn!("the task stopping the watcher failed: {e}");
            }
            self.watcher_stop = None;
        }
        if let Some(task) = self.sign_in.as_mut() {
            let _ = task.await;
            self.sign_in = None;
        }
        if let Some(task) = self.thumbnails.as_mut() {
            let _ = task.await;
            self.thumbnails = None;
        }
        loop {
            if self.tidied.is_none() {
                self.tidied = crate::panic::lock(&self.tidying).pop();
            }
            let Some(task) = self.tidied.as_mut() else { break };
            let _ = task.await;
            self.tidied = None;
        }
    }
}

impl Ended {
    /// Waits for every part that was told to stop. Cut while it waits, nothing is lost:
    /// the parts not yet waited for stay, for the next call.
    pub async fn join(&self) {
        loop {
            let Some(ending) = crate::panic::lock(&self.0).pop() else { return };
            let mut waited = Waited { ending: Some(ending), ended: self };
            if let Some(ending) = waited.ending.as_mut() {
                ending.join().await;
            }
            waited.ending = None;
        }
    }
}

/// An [`Ending`] while it is waited for: put back when the wait is cut.
struct Waited<'a> {
    ending: Option<Ending>,
    ended: &'a Ended,
}

impl Drop for Waited<'_> {
    fn drop(&mut self) {
        if let Some(ending) = self.ending.take() {
            crate::panic::lock(&self.ended.0).push(ending);
        }
    }
}
