//! Changes from OneDrive at once: one task per account, started and stopped with
//! its poller, keeps Graph's notification socket open ([`konedrive_graph::drive::socket`]) and asks the
//! poller for a cycle when an event says the drive changed. The poll stays as the safety net:
//! every [`Schedule::live_interval`](super::listing::Schedule::live_interval) while the socket
//! is up, every `interval` otherwise (`docs/design/sync.md`, "Changes as they happen").
//!
//! - While the account is stopped (the user's pause or the automatic hold, `running`) no
//!   connection is kept; a stop that comes while connected closes it at once. The task is
//!   woken by the same changes that nudge the poller ([`Live::wake`]), with a wake-up of its
//!   own.
//! - Events within [`Timing::debounce`] of the first give one cycle.
//! - The endpoint is fetched again, and a new connection opened before the old one is
//!   closed, [`RENEW_EARLY`](konedrive_graph::drive::socket::RENEW_EARLY) before it expires; the
//!   renewal asks for one cycle, since the old socket was not read while the new one opened.
//!   The deadline is also kept as wall-clock time, so a machine that slept past it renews at
//!   its first wake-up.
//! - A connection counts as up only after the server's first ping or [`Timing::settle`];
//!   one that ends sooner is a failure like one that never opened (no reconnect storm).
//! - A connection that ends is tried again after 1, 2, 4 … 60 s (`Timing`), and the poller is
//!   told at once that the socket is down; the first connection up after such a drop asks
//!   for one cycle, since events during the gap are lost.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{watch, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::conditions::running::Running;
use crate::status::snapshot::{LiveChanges, SyncStateHandle};

use konedrive_graph::drive::socket::{Heard, NotificationSocket, SocketEndpoint};
use konedrive_graph::drive::DriveClient;
use konedrive_tree::Store;

/// The live task's waits.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Events within this of the first give one cycle.
    pub debounce: Duration,
    /// The first wait after a failure; each one after doubles it, up to `backoff_max`.
    pub backoff: Duration,
    pub backoff_max: Duration,
    /// The shortest time an endpoint is kept before it is renewed, whatever its expiry says:
    /// an endpoint that expires within `RENEW_EARLY` would otherwise be renewed in a loop.
    pub renew_floor: Duration,
    /// While the account is stopped, how often it is looked at again besides the wake-ups.
    pub stopped_look: Duration,
    /// A connection counts as up after the server's first ping, or after this much life
    /// without one; one that ends sooner counts as a failure.
    pub settle: Duration,
    /// The wall clock, for the renewal deadline (the tests move it).
    pub clock: fn() -> SystemTime,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            backoff: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            renew_floor: Duration::from_secs(60),
            stopped_look: Duration::from_secs(60),
            settle: Duration::from_secs(30),
            clock: SystemTime::now,
        }
    }
}

/// What the live task works with: the account's drive and what decides whether it runs, the
/// poller's wake-up and the flag the poller reads.
pub struct LiveContext {
    pub drive: DriveClient,
    pub store: Store,
    pub running: Arc<Running>,
    pub state: SyncStateHandle,
    /// The poller's `refresh`: a cycle now.
    pub refresh: Arc<Notify>,
    /// Whether the socket is up, for the poller's interval.
    pub up: Arc<watch::Sender<bool>>,
}

impl LiveContext {
    fn stopped(&self) -> bool {
        self.running.stopped()
    }

    fn show(&self, live: LiveChanges) {
        self.state.set_live_changes(live);
    }

    fn set_up(&self, up: bool) {
        self.up.send_if_modified(|was| std::mem::replace(was, up) != up);
    }
}

/// The running live task of one account.
pub struct Live {
    wake: Arc<Notify>,
    /// `None` once it has been waited for.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Live {
    /// Starts the task; it ends when `cancel` does (the poller's token).
    pub fn start(ctx: LiveContext, timing: Timing, cancel: CancellationToken) -> Self {
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run(ctx, timing, Arc::clone(&wake), cancel));
        Self { wake, task: Some(task) }
    }

    /// What wakes the task ([`wake`](Self::wake)), for whoever does not own it.
    pub fn waker(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// The pause, the hold or the network may have changed: the task looks again at once.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Waits for the task, once its token is cancelled.
    pub async fn join(&mut self) {
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
            self.task = None;
        }
    }
}

/// The bound on closing a socket politely.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How a connection ended.
enum End {
    Cancelled,
    /// The account stopped: closed on purpose.
    Stopped,
    /// Dropped, refused, gone quiet, or its endpoint could not be renewed.
    Lost(String),
}

async fn run(ctx: LiveContext, timing: Timing, wake: Arc<Notify>, cancel: CancellationToken) {
    let mut failures = 0u32;
    // A connection was up and ended: the next one asks for a cycle (events were lost).
    let mut dropped = false;
    loop {
        if ctx.stopped() {
            ctx.show(LiveChanges::Off);
            (failures, dropped) = (0, false);
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(timing.stopped_look) => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        ctx.show(LiveChanges::Connecting);
        let opened = tokio::select! {
            opened = open(&ctx.drive) => opened,
            () = cancel.cancelled() => return,
        };
        let (endpoint, socket) = match opened {
            Ok(opened) => opened,
            Err(why) => {
                said(failures, &why);
                if !back_off(&timing, &mut failures, &wake, &cancel).await {
                    return;
                }
                continue;
            }
        };
        if ctx.stopped() {
            close(socket).await;
            continue;
        }
        let mut link = Link { ctx: &ctx, failures: &mut failures, dropped: &mut dropped, up: false };
        let end = serve(&mut link, &timing, endpoint, socket, &wake, &cancel).await;
        let was_up = link.up;
        ctx.set_up(false);
        match end {
            End::Cancelled => return,
            End::Stopped => tracing::info!("the notification socket is closed while the account's sync stops"),
            End::Lost(why) => {
                if was_up {
                    tracing::warn!("{why}; the poll carries on meanwhile");
                    dropped = true;
                } else {
                    // Ended before it was up: a failure in a row, the backoff keeps growing.
                    said(failures, &format!("{why} (before the connection was up)"));
                }
                ctx.show(LiveChanges::Connecting);
                if !back_off(&timing, &mut failures, &wake, &cancel).await {
                    return;
                }
            }
        }
    }
}

/// One connection's standing in the task: whether it is up yet, and the task's counters it
/// resets once it is.
struct Link<'a> {
    ctx: &'a LiveContext,
    failures: &'a mut u32,
    dropped: &'a mut bool,
    up: bool,
}

impl Link<'_> {
    /// The connection proved alive (a ping, or `settle` of life): it counts as up.
    fn go_up(&mut self, endpoint: &SocketEndpoint) {
        if std::mem::replace(&mut self.up, true) {
            return;
        }
        *self.failures = 0;
        tracing::info!(host = %endpoint.host(), "changes from OneDrive arrive as they happen");
        self.ctx.set_up(true);
        self.ctx.show(LiveChanges::Connected);
        if std::mem::take(self.dropped) {
            self.ctx.refresh.notify_one();
        }
    }
}

/// Closes a socket politely, but never waits more than [`CLOSE_TIMEOUT`] for it.
async fn close(socket: NotificationSocket) {
    if tokio::time::timeout(CLOSE_TIMEOUT, socket.close()).await.is_err() {
        tracing::debug!("the notification socket did not close within {CLOSE_TIMEOUT:?}; dropped");
    }
}

/// The endpoint, and a connection to it.
async fn open(drive: &DriveClient) -> Result<(SocketEndpoint, NotificationSocket), String> {
    let endpoint = drive.socket_endpoint().await.map_err(|e| format!("cannot get the notification endpoint: {e}"))?;
    let socket = NotificationSocket::connect(&endpoint.notification_url).await.map_err(|e| e.to_string())?;
    Ok((endpoint, socket))
}

/// Said once per run of failures, then only at `debug`: a machine without a direct route (a
/// proxy, issue #209) fails every minute for good.
fn said(failures: u32, why: &str) {
    if failures == 0 {
        tracing::warn!("{why}; changes from OneDrive are polled for meanwhile");
    } else {
        tracing::debug!("{why}");
    }
}

/// Waits 1, 2, 4 … s after the `failures`-th failure in a row, or until a wake-up. False when
/// cancelled.
async fn back_off(timing: &Timing, failures: &mut u32, wake: &Notify, cancel: &CancellationToken) -> bool {
    let wait = timing.backoff.saturating_mul(1u32 << (*failures).min(16)).min(timing.backoff_max);
    *failures = failures.saturating_add(1);
    tokio::select! {
        () = tokio::time::sleep(wait) => true,
        () = wake.notified() => true,
        () = cancel.cancelled() => false,
    }
}

/// Keeps one connection: it counts as up at its first ping (or after `settle`), events
/// become cycles, the endpoint is renewed before it expires, and a stop closes it.
async fn serve(
    link: &mut Link<'_>,
    timing: &Timing,
    mut endpoint: SocketEndpoint,
    mut socket: NotificationSocket,
    wake: &Notify,
    cancel: &CancellationToken,
) -> End {
    let ctx = link.ctx;
    let clock = timing.clock;
    // The renewal deadline on both clocks: the monotonic one does not count a suspend.
    let deadlines = |endpoint: &SocketEndpoint| {
        let wait = endpoint.renew_after(clock()).max(timing.renew_floor);
        (Instant::now() + wait, clock() + wait)
    };
    let (mut renew, mut renew_wall) = deadlines(&endpoint);
    let settled = Instant::now() + timing.settle;
    // When the cycle asked for by the first event of a burst is due.
    let mut due: Option<Instant> = None;
    let end = loop {
        // Any wake-up (a ping, an event, a nudge) after a suspend past the deadline renews.
        if clock() >= renew_wall {
            renew = Instant::now();
        }
        tokio::select! {
            got = socket.heard() => match got {
                Ok(Heard::Notification) => {
                    due.get_or_insert_with(|| Instant::now() + timing.debounce);
                }
                Ok(Heard::Ping) => link.go_up(&endpoint),
                Err(why) => break End::Lost(why.to_string()),
            },
            () = tokio::time::sleep_until(settled), if !link.up => link.go_up(&endpoint),
            () = sleep_until(due), if due.is_some() => {
                due = None;
                ctx.refresh.notify_one();
            }
            () = tokio::time::sleep_until(renew) => {
                let opened = tokio::select! {
                    opened = open(&ctx.drive) => opened,
                    () = cancel.cancelled() => break End::Cancelled,
                };
                match opened {
                    // The new one first, then the old one goes. The old one was not read
                    // while the new one opened, so an event may have been missed: one cycle.
                    Ok((fresh_endpoint, fresh)) => {
                        close(std::mem::replace(&mut socket, fresh)).await;
                        endpoint = fresh_endpoint;
                        (renew, renew_wall) = deadlines(&endpoint);
                        tracing::debug!(host = %endpoint.host(), "the notification endpoint was renewed");
                        ctx.refresh.notify_one();
                    }
                    Err(why) => break End::Lost(format!("the notification endpoint could not be renewed: {why}")),
                }
            }
            () = wake.notified() => {
                if ctx.stopped() {
                    break End::Stopped;
                }
            }
            () = cancel.cancelled() => break End::Cancelled,
        }
    };
    // An event already heard still gets its cycle.
    if due.is_some() {
        ctx.refresh.notify_one();
    }
    if !matches!(end, End::Lost(_)) {
        close(socket).await;
    }
    end
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
