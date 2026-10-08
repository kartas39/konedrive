//! The running sync of a OneDrive folder, as one object in the folder's state
//! ([`RunningSync`]): it owns its parts — the poller, the outbox worker of a read-write
//! folder, the thumbnail filler, the sign-in nudge. The tree store is the
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
    /// Nothing has started it yet: the folder came up a moment ago.
    NotStarted,
    /// A change of the folder stopped it and has not started it again: the change is
    /// under way, or was cut before its end. The next change starts it.
    Interrupted,
    /// It could not start. The sentence; `Refresh()` tries again.
    CannotStart(String),
}

/// What a reader may reach of a running sync: what wakes its parts or tells them to stop. Nothing here can wait for a part; only the [`RunningSync`] owns them.
#[derive(Clone)]
pub(super) struct Handles {
    pub poll: PollHandle,
    /// The outbox worker of a read-write folder.
    pub outbox: Option<OutboxHandle>,
    /// `NotUploadedSummary()` as the outbox worker last summed it: answered
    /// from memory while the worker runs.
    pub kept_back: Arc<Mutex<Option<Vec<SummaryRow>>>>,
    /// Stops the tasks that have no handle of their own: the sign-in nudge and the
    /// thumbnail filler.
    pub(super) stop: CancellationToken,
}

impl Handles {
    /// Tells every part to stop, at once, without waiting for any.
    pub fn cancel(&self) {
        self.stop.cancel();
        self.poll.cancel();
        if let Some(outbox) = &self.outbox {
            outbox.cancel();
        }
    }

    /// Whether the sync was told to stop ([`cancel`](Self::cancel)): its parts end, or
    /// have ended.
    pub fn told_to_stop(&self) -> bool {
        self.stop.is_cancelled()
    }

    pub fn outbox(&self) -> Option<&OutboxHandle> {
        self.outbox.as_ref()
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
    /// The outbox worker of a read-write folder: what programs change in the folder is
    /// recorded by its mount, and sent by this. A read-only folder has none, and its mount
    /// takes no change.
    pub outbox: Option<OutboxWorker>,
}

impl RunningSync {
    pub fn new(handles: Handles, parts: Parts, ended: Ended) -> Self {
        Self { handles, parts: Some(parts), ended }
    }

    pub fn handles(&self) -> &Handles {
        &self.handles
    }

}

impl Drop for RunningSync {
    fn drop(&mut self) {
        self.handles.cancel();
        if let Some(parts) = self.parts.take() {
            crate::panic::lock(&self.ended.0).push(Ending {
                poller: Some(parts.poller),
                outbox: parts.outbox,
                sign_in: Some(parts.sign_in),
                thumbnails: parts.thumbnails,
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
    sign_in: Option<JoinHandle<()>>,
    thumbnails: Option<JoinHandle<()>>,
}

impl Ending {
    /// Waits for every part, in the order the locks ask for. A part that has been waited
    /// for is let go; cut while it waits, this goes on from that part at the next call.
    async fn join(&mut self) {
        // The poller first: a cycle may hold the tree lock, which the worker's commit waits
        // for.
        if let Some(poller) = self.poller.as_mut() {
            poller.join().await;
            self.poller = None;
        }
        // The outbox worker next: a request under way was cut off, and its row is replayed
        // when a worker starts again (`docs/design/writes.md` §10).
        if let Some(outbox) = self.outbox.as_ref() {
            outbox.stop().await;
            self.outbox = None;
        }
        if let Some(task) = self.sign_in.as_mut() {
            let _ = task.await;
            self.sign_in = None;
        }
        if let Some(task) = self.thumbnails.as_mut() {
            let _ = task.await;
            self.thumbnails = None;
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
