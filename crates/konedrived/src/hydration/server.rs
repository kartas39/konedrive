use std::os::fd::AsFd;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::FutureExt;

use crate::status::activity::Kind;
use crate::status::report::Report;
use crate::hydration::tracked::Tracked;
use crate::helper::{HelperLink, HydrateRequest};
use crate::hydration::source::{Answered, ContentSource, FillError};
use crate::folder::locks::{InodeKey, InodeLocks, unless_removed};
use crate::hydration::source;
use crate::status::activity;

/// Hydration requests taken off the queue at once: the helper's whole credit
/// (`konedrive_proto::MAX_OUTSTANDING_HYDRATIONS`). Each is routed to its account and then
/// waits for a slot of that account's transfer pool (`konedrive_graph::pool`, `Class::Open`).
pub const FILL_ADMISSION: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;

/// Answers hydration requests until the helper goes away. Each request is routed
/// to its account first, then takes a slot of that account's transfer pool — an
/// open goes before any background work and may use the pool's reserve — and no
/// request is ever dropped silently.
///
/// An admission permit ([`FILL_ADMISSION`]) is acquired *before* spawning, not
/// inside the spawned task. Acquiring nothing would drain the bounded mpsc
/// of hydration requests into an unbounded pile of tasks — each holding a
/// suspended open's event descriptor — as fast as the helper could send
/// them, destroying the backpressure the channel exists to provide. The pool's
/// slot is taken inside the task, after routing, so that one account's full pool
/// never holds up another account's open.
/// Blocking here, before `recv()` is called again, propagates that
/// backpressure all the way back to the helper — but only as far as the
/// request queue. It must never reach the socket: the reader thread that
/// fills the queue is also the one that reads the `Ack` each fill below
/// waits for before it lets go of its permit, so a reader stopped by a full
/// queue with requests still ahead of an `Ack` in the socket wedges the
/// fills for good. The helper keeps at most
/// `konedrive_proto::MAX_OUTSTANDING_HYDRATIONS` requests outstanding on a
/// connection and the queue is exactly that deep, so the reader never stops;
/// beyond that the helper holds further hydrations back itself, and sends
/// each as one of these finishes.
///
/// Every fill is tracked in a `JoinSet` and the set is drained before this
/// returns, so a shutdown (or a helper that disconnects) lets the fills that
/// are already running finish and answer, rather than cutting them mid-write
/// and leaving `state=hydrating` behind on disk. That drain can take as long
/// as a download, so nothing that must react to the connection ending may
/// wait for this to return: the hub's supervisor runs it as a task of its
/// own and waits on [`HelperLink::closed`] instead.
///
/// # Per-inode serialization
///
/// Dehydration (`docs/design/hydration.md` §8) and a fill (§6.1) take the per-inode lock: the daemon
/// serializes operations per inode, so a
/// hydration request for a file being dehydrated runs after the dehydration
/// finishes. `locks` is what keeps that promise: this loop and
/// `SyncService::dehydrate` run in the same daemon and share the one table,
/// so two fills of the *same* file — one a hydration, one a dehydration's
/// punch — never run concurrently and tear it.
///
/// `locks` is keyed by `(st_dev, st_ino)` read from the descriptor itself,
/// which is what "per inode" means and the only key the
/// two sides can be made to agree on. A key made of a path string —
/// `readlink("/proc/self/fd/<n>")` here, `canonicalize()` on the D-Bus side —
/// does not serialize two names for one inode at all: measured, `ln f.bin
/// g.bin` plus one `Hydrate` call on each name put **two fills in flight on
/// the same inode**, where a failing fetch's roll-back (`online-only` +
/// `punch_all`) lands on top of the other fill's committed `hydrated`,
/// leaving a file labelled `hydrated` over a hole — which the helper then
/// allows *and* ignore-marks. `readlink` also answers `"<path> (deleted)"`
/// for an unlinked file, and any rename between the two sides' key
/// computations desynchronises them.
pub async fn serve_hydrations(
    link: HelperLink,
    requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    source: Arc<dyn ContentSource>,
    locks: InodeLocks,
) {
    let nowhere = Report::nowhere();
    serve_hydrations_reporting(link, requests, source, locks, nowhere).await;
}

/// [`serve_hydrations`], reporting each fill into `report`: a
/// `Transfers` entry while it downloads, then a `downloaded` or `failed`
/// event, and a new measurement of the folder's space. The daemon runs the
/// same loop with every fill routed to its account (`helper::hub::supervise`);
/// what a fill answers the opener is the same either way, and it is
/// answered before anything is recorded.
pub async fn serve_hydrations_reporting(
    link: HelperLink,
    requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    source: Arc<dyn ContentSource>,
    locks: InodeLocks,
    report: Report,
) {
    let pool = konedrive_graph::pool::TransferPool::new(konedrive_graph::pool::DEFAULT_CEILING);
    serve(link, requests, locks, Fillers::One(source, report, pool)).await;
}

/// What fills a hydration request: its source, where it is reported, and the transfer pool
/// it takes a slot of.
pub(crate) type Filler = (Arc<dyn ContentSource>, Report, Arc<konedrive_graph::pool::TransferPool>);

/// Says which account an open file belongs to. The registry of the daemon's folders
/// implements it (`sync::registry::Registry`).
#[async_trait]
pub(crate) trait Router: Send + Sync {
    /// What fills the file `fd` is open on; `None` when it is in no account's folder.
    async fn route(&self, fd: &std::os::fd::OwnedFd) -> Option<Filler>;
}

/// Who fills a hydration request, and where it is reported.
#[derive(Clone)]
pub(crate) enum Fillers {
    /// One source, one report, one pool, whatever the file (tests, the VM suite).
    One(Arc<dyn ContentSource>, Report, Arc<konedrive_graph::pool::TransferPool>),
    /// The account the file belongs to ([`Router::route`]): the daemon's.
    Routed(Arc<dyn Router>),
}

impl Fillers {
    async fn route(&self, fd: &std::os::fd::OwnedFd) -> Option<Filler> {
        match self {
            Fillers::One(source, report, pool) => Some((Arc::clone(source), report.clone(), Arc::clone(pool))),
            Fillers::Routed(router) => router.route(fd).await,
        }
    }
}

/// The loop behind [`serve_hydrations_reporting`] and the hub's: at most
/// [`FILL_ADMISSION`] requests taken at once, each filled in a slot of its account's pool.
pub(crate) async fn serve(
    link: HelperLink,
    mut requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    locks: InodeLocks,
    fillers: Fillers,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(FILL_ADMISSION));
    let mut running = tokio::task::JoinSet::new();
    while let Some(HydrateRequest { req_id, fd }) = requests.recv().await {
        // Reap whatever finished while we were waiting; the set must not
        // accumulate the results of completed fills for the life of the
        // daemon.
        while running.try_join_next().is_some() {}
        // A request that arrives, or reaches the front, after its
        // connection ended is not filled: the helper answered
        // its opener `EIO` when the connection went (its disconnect guard
        // takes every job the connection had), so a fill would download a
        // file for nobody — and, while the 64 requests taken at once are all held, keep this
        // loop from noticing the end at all.
        let permit = tokio::select! {
            permit = Arc::clone(&permits).acquire_owned() => permit.expect("semaphore closed"),
            () = link.closed() => {
                tracing::warn!(
                    "hydration request {req_id} came from a helper connection that has ended; its \
                     opener was answered then, so it is not filled"
                );
                continue;
            }
        };
        if link.is_closed() {
            tracing::warn!(
                "hydration request {req_id} came from a helper connection that has ended; its \
                 opener was answered then, so it is not filled"
            );
            continue;
        }
        let link = link.clone();
        let fillers = fillers.clone();
        let locks = locks.clone();
        running.spawn(async move {
            let permit = permit;
            // Which account's file this is (`docs/design/accounts.md` §3.4). One that is in no
            // account's folder is denied rather than filled from a guess;
            // the next open tries again.
            let Some((source, report, pool)) = fillers.route(&fd).await else {
                tracing::warn!(
                    "hydration request {req_id} is for a file in none of the folders ({}); \
                     denying that open with EIO",
                    fd_path(&fd)
                );
                if let Err(e) = link.hydrate_done(req_id, libc::EIO).await {
                    tracing::error!("cannot report hydration {req_id}: {e}");
                }
                drop(permit);
                return;
            };
            // Routed first, then a slot of that account's pool: an open goes before
            // any background work there, and may use the pool's reserve. A connection
            // that ends meanwhile had its opener answered by the helper. The placeholder's
            // size says whether it is a large transfer: counted as one, never held by the
            // large-file limit.
            let bytes = nix::sys::stat::fstat(&fd).map_or(0, |stat| stat.st_size.max(0) as u64);
            let mut slot = tokio::select! {
                slot = pool.acquire_sized(konedrive_graph::pool::Class::Open, konedrive_graph::pool::Size::of(bytes)) => slot,
                () = link.closed() => {
                    tracing::warn!("hydration request {req_id} waited for a transfer slot until its helper connection ended; not filled");
                    return;
                }
            };
            // The identity the lock is taken on: `fstat` on the event fd
            // itself, read before the fd is handed to `hydrate` (which
            // consumes it). A descriptor whose identity cannot be read at
            // all still gets filled — nothing here is dropped for the sake
            // of the lock — but that is a degradation, not a detail, so it
            // is logged rather than silently taken (the version this
            // replaces read `/proc/self/fd/<n>` and said nothing when the
            // readlink failed).
            let key = match InodeKey::of_fd(&fd) {
                Ok(key) => Some(key),
                Err(e) => {
                    tracing::warn!(
                        "cannot read the identity of the file in request {req_id}: {e}; filling \
                         it without the per-inode lock, so a concurrent dehydration of the same \
                         file is not serialized against this fill"
                    );
                    None
                }
            };
            let inode_guard = match key {
                Some(key) => Some(locks.lock(key).await),
                None => None,
            };
            // Only what is shown: the name the kernel has for
            // the file right now, read before `answer_request` takes the fd.
            let shown = fd_path(&fd);
            let tracked = Tracked::opening(Arc::clone(&source), report.transfers.clone(), shown.clone());
            // A panic anywhere in the fill — including inside a
            // `ContentSource` we did not write — must not become an
            // unanswerable event in the kernel. Unwinding out of here would
            // close the event fd and produce no errno at all, so
            // `hydrate_done` would never be called and the suspended
            // `open()` would wait forever: hydration.md §5.2's 30 s bound covers only "the
            // owner's daemon is not connected", and this daemon is connected.
            // Degrading it to an `EIO` denial costs the user one failed open.
            //
            // What the request finds under the lock decides what it does:
            // a file filled while the request waited is
            // answered as it is — see `source::answer_request`.
            //
            // A file taken off the disk meanwhile, because OneDrive removed
            // its item, stops its fill where it is. Its opener is
            // answered `EIO`: the kernel delivers no errno that says "gone".
            // `ENOENT` is not in `ACCEPTED_DENY_ERRNOS`, and the helper
            // turns an errno outside that set into `EIO`, with a warning.
            let filled = unless_removed(inode_guard.as_ref(), AssertUnwindSafe(source::answer_request(fd, &tracked, Some(&link))).catch_unwind()).await;
            let size = tracked.fetched();
            // Whatever came of it, the download is over.
            drop(tracked);
            let (errno, event) = match filled {
                None => {
                    tracing::info!("the hydration of request {req_id} stopped: its file was removed in OneDrive");
                    (libc::EIO, Some(activity::event(Kind::Failed, shown, "removed in OneDrive".to_owned())))
                }
                Some(Ok(answered)) => {
                    if matches!(answered, Answered::Filled) {
                        slot.succeeded();
                    }
                    (answered.errno(), fill_event(&answered, &shown, size))
                }
                Some(Err(_)) => {
                    tracing::error!(
                        "the hydration of request {req_id} panicked; denying that open with EIO \
                         rather than leaving it suspended forever"
                    );
                    (libc::EIO, Some(activity::event(Kind::Failed, shown, activity::failure_reason(libc::EIO))))
                }
            };
            if let Err(e) = link.hydrate_done(req_id, errno).await {
                tracing::error!("cannot report hydration {req_id}: {e}");
            }
            // The slot goes back before anything is recorded: a record that
            // waits (the log is SQLite) must not keep the next request from
            // being filled.
            drop(inode_guard);
            drop(slot);
            drop(permit);
            if let Some(event) = event {
                report.activity.record(vec![event]).await;
                report.space.kick();
            }
        });
    }
    while running.join_next().await.is_some() {}
}

/// The name the kernel has for an open file, for showing it:
/// `/proc/self/fd/<n>`, read, never followed. Empty if it cannot be read.
fn fd_path(fd: &impl AsFd) -> String {
    std::fs::read_link(konedrive_fs::proc_path(fd))
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

/// What a fill of `path` records: `downloaded` with its size
/// when something was downloaded, `failed` with why when a fill ran and
/// failed, and nothing when there was nothing to do.
pub(crate) fn fill_event(answered: &Answered, path: &str, size: Option<u64>) -> Option<activity::Event> {
    match answered {
        Answered::Filled => Some(activity::event(Kind::Downloaded, path, activity::human_size(size.unwrap_or(0)))),
        Answered::Failed(FillError::Errno(errno)) => Some(activity::event(Kind::Failed, path, activity::failure_reason(*errno))),
        Answered::Failed(FillError::NotCleared(why)) => Some(activity::event(Kind::Failed, path, why.to_string())),
        Answered::AlreadyThere | Answered::NotOurs => None,
    }
}

#[cfg(test)]
mod tests;
