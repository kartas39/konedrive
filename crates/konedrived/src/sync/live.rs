//! Changes from OneDrive at once (issue #54): one task per account, started and stopped with
//! its poller, keeps Graph's notification socket open ([`crate::drive::socket`]) and asks the
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
//!   closed, [`RENEW_EARLY`](crate::drive::socket::RENEW_EARLY) before it expires; the
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

use super::running::Running;
use super::SyncStateHandle;
use crate::drive::socket::{Heard, NotificationSocket, SocketEndpoint};
use crate::drive::DriveClient;
use crate::tree::Store;

/// `LiveChanges` on the bus: how changes made in OneDrive reach this computer now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveChanges {
    /// No socket: the account is stopped (pause or hold), or the folder is not a OneDrive
    /// folder, or no sync runs.
    #[default]
    Off,
    /// Trying to connect, or waiting before the next try: the poll runs at its normal interval.
    Connecting,
    /// The socket is up: changes arrive at once.
    Connected,
}

impl LiveChanges {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
}

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
        self.running.stopped(&self.store)
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
    task: tokio::task::JoinHandle<()>,
}

impl Live {
    /// Starts the task; it ends when `cancel` does (the poller's token).
    pub fn start(ctx: LiveContext, timing: Timing, cancel: CancellationToken) -> Self {
        let wake = Arc::new(Notify::new());
        let task = tokio::spawn(run(ctx, timing, Arc::clone(&wake), cancel));
        Self { wake, task }
    }

    /// The pause, the hold or the network may have changed: the task looks again at once.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Waits for the task, once its token is cancelled.
    pub async fn join(self) {
        let _ = self.task.await;
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
/// proxy, limitations log) fails every minute for good.
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
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;
    use crate::sync::running::Conditions;
    use crate::sync::upload::fake::{Early, FakeGraph, ROOT};
    use crate::sync::SyncSnapshot;
    use crate::tree::TreeStore;

    const DEBOUNCE: Duration = Duration::from_millis(300);

    /// One account's live task against the fake OneDrive, and the cycles it asked for.
    struct World {
        graph: FakeGraph,
        store: Store,
        running: Arc<Running>,
        state: SyncStateHandle,
        up: Arc<watch::Sender<bool>>,
        cycles: Arc<AtomicUsize>,
        cancel: CancellationToken,
        live: Option<Live>,
        files: usize,
    }

    async fn world() -> World {
        world_with(|_| {}).await
    }

    async fn world_with(setup: impl FnOnce(&mut crate::sync::upload::fake::Cloud)) -> World {
        world_timed(setup, |_| {}).await
    }

    async fn world_timed(setup: impl FnOnce(&mut crate::sync::upload::fake::Cloud), adjust: impl FnOnce(&mut Timing)) -> World {
        let graph = FakeGraph::start().await;
        graph.with(setup);
        let store = Store::new(TreeStore::in_memory().unwrap());
        let running = Arc::new(Running::default());
        let state = SyncStateHandle::new(SyncSnapshot::default());
        let refresh = Arc::new(Notify::new());
        let cycles = Arc::new(AtomicUsize::new(0));
        tokio::spawn({
            let (refresh, cycles) = (Arc::clone(&refresh), Arc::clone(&cycles));
            async move {
                loop {
                    refresh.notified().await;
                    cycles.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let up = Arc::new(watch::channel(false).0);
        let cancel = CancellationToken::new();
        let ctx = LiveContext {
            drive: graph.client(),
            store: store.clone(),
            running: Arc::clone(&running),
            state: state.clone(),
            refresh,
            up: Arc::clone(&up),
        };
        let mut timing = Timing {
            debounce: DEBOUNCE,
            backoff: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            renew_floor: Duration::from_millis(200),
            stopped_look: Duration::from_secs(3600),
            // The fake pings every 25 s: up after this instead.
            settle: Duration::from_millis(100),
            clock: SystemTime::now,
        };
        adjust(&mut timing);
        let live = Some(Live::start(ctx, timing, cancel.clone()));
        World { graph, store, running, state, up, cycles, cancel, live, files: 0 }
    }

    impl World {
        fn live(&self) -> LiveChanges {
            self.state.get().live_changes
        }

        fn cycles(&self) -> usize {
            self.cycles.load(Ordering::SeqCst)
        }

        fn endpoints(&self) -> usize {
            self.graph.with(|c| c.count("GET", "subscriptions/socketIo"))
        }

        /// A new file in OneDrive, made on another device.
        fn change(&mut self) {
            self.files += 1;
            let (id, name) = (format!("X{}", self.files), format!("x{}.txt", self.files));
            self.graph.with(|c| c.add_file(&id, ROOT, &name, b"x"));
        }

        fn wake(&self) {
            self.live.as_ref().unwrap().wake();
        }

        async fn wait(&self, what: &str, done: impl Fn(&Self) -> bool) {
            for _ in 0..250 {
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("{what}: not within 5 s (live {:?}, {} cycles)", self.live(), self.cycles());
        }

        async fn connected(&self) {
            self.wait("connected", |w| w.live() == LiveChanges::Connected && *w.up.borrow() && w.graph.sockets.open() == 1).await;
        }

        async fn stop(mut self) {
            self.cancel.cancel();
            tokio::time::timeout(Duration::from_secs(5), self.live.take().unwrap().join()).await.expect("the task ends");
        }
    }

    #[tokio::test]
    async fn an_event_asks_for_one_cycle_after_the_debounce_and_so_does_a_burst() {
        let mut w = world().await;
        w.connected().await;
        assert_eq!(w.cycles(), 0, "the first connection asks for nothing: the poller's first cycle runs anyway");

        let changed = Instant::now();
        w.change();
        w.wait("a cycle for the event", |w| w.cycles() == 1).await;
        assert!(changed.elapsed() >= DEBOUNCE, "after the debounce: {:?}", changed.elapsed());

        for _ in 0..5 {
            w.change();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        w.wait("a cycle for the burst", |w| w.cycles() == 2).await;
        tokio::time::sleep(DEBOUNCE * 2).await;
        assert_eq!(w.cycles(), 2, "one cycle for the whole burst");
        w.stop().await;
    }

    #[tokio::test]
    async fn a_drop_is_told_at_once_and_the_reconnection_after_it_asks_for_one_cycle() {
        let w = world().await;
        w.connected().await;
        w.graph.sockets.refuse(true);
        w.graph.sockets.drop_all();
        w.wait("down", |w| w.live() == LiveChanges::Connecting && !*w.up.borrow()).await;
        let tried = w.graph.sockets.accepted();
        w.wait("tried again, on the backoff", |w| w.graph.sockets.accepted() >= tried + 2).await;
        assert_eq!(w.cycles(), 0, "nothing reconnected yet");
        assert_eq!(w.live(), LiveChanges::Connecting);

        w.graph.sockets.refuse(false);
        w.connected().await;
        w.wait("the cycle for the gap", |w| w.cycles() == 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(w.cycles(), 1, "one cycle for the gap");
        w.stop().await;
    }

    #[tokio::test]
    async fn the_endpoint_is_renewed_before_it_expires_and_each_renewal_asks_for_a_cycle() {
        // Expires 121 s after it is handed out: renewed after about a second (`RENEW_EARLY`).
        let w = world_with(|c| c.socket_lifetime = Some(121)).await;
        w.connected().await;
        w.wait("renewed twice", |w| w.endpoints() >= 3 && w.graph.sockets.accepted() >= 3).await;
        w.wait("the old connections closed", |w| w.graph.sockets.open() == 1).await;
        assert_eq!(w.live(), LiveChanges::Connected, "connected all along");
        w.wait("a cycle per renewal: the old socket was not read meanwhile", |w| w.cycles() >= 2).await;
        w.stop().await;
    }

    /// How far the moved wall clock is ahead of the real one, in seconds.
    static AHEAD: AtomicU64 = AtomicU64::new(0);

    fn moved_clock() -> SystemTime {
        SystemTime::now() + Duration::from_secs(AHEAD.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_wake_up_past_the_wall_clock_deadline_renews_as_after_a_suspend() {
        // Renewed in about 58 min by the monotonic clock, which does not count a suspend.
        let w = world_timed(|c| c.socket_lifetime = Some(3600), |t| t.clock = moved_clock).await;
        w.connected().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(w.endpoints(), 1, "not renewed yet");

        // The machine slept for an hour: the wall clock is past the deadline.
        AHEAD.store(3600, Ordering::SeqCst);
        w.wake();
        w.wait("renewed at the wake-up", |w| w.endpoints() >= 2 && w.graph.sockets.accepted() >= 2).await;
        w.wait("a cycle for the renewal", |w| w.cycles() >= 1).await;
        assert_eq!(w.live(), LiveChanges::Connected);
        w.stop().await;
    }

    #[tokio::test]
    async fn connections_dropped_before_they_are_up_back_off_further_and_ask_for_no_cycle() {
        for early in [Early::Refuse, Early::Close] {
            // Up only after 5 s: every connection here ends long before.
            let w = world_timed(|_| {}, |t| {
                t.backoff_max = Duration::from_secs(10);
                t.settle = Duration::from_secs(5);
            })
            .await;
            w.graph.sockets.early(early);
            // Waits of 50, 100, 200, 400, 800 ms: about six tries in 1.6 s, not thirty.
            tokio::time::sleep(Duration::from_millis(1600)).await;
            let tried = w.graph.sockets.accepted();
            assert!((3..=7).contains(&tried), "{early:?}: a growing backoff, {tried} connections");
            assert_eq!(w.cycles(), 0, "{early:?}: no cycle for a connection that was never up");
            assert_eq!(w.live(), LiveChanges::Connecting, "{early:?}");
            assert!(!*w.up.borrow(), "{early:?}: the poller never told it is up");
            w.stop().await;
        }
    }

    #[tokio::test]
    async fn a_pause_and_a_hold_close_the_socket_and_keep_it_closed_until_they_end() {
        let w = world().await;
        w.connected().await;

        crate::sync::upload::set_paused(&w.store, Some(0)).await.unwrap();
        w.wake();
        w.wait("closed by the pause", |w| w.live() == LiveChanges::Off && w.graph.sockets.open() == 0 && !*w.up.borrow()).await;
        let (endpoints, accepted) = (w.endpoints(), w.graph.sockets.accepted());
        w.wake();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!((w.endpoints(), w.graph.sockets.accepted()), (endpoints, accepted), "nothing is asked while paused");
        crate::sync::upload::set_paused(&w.store, None).await.unwrap();
        w.wake();
        w.connected().await;

        w.running.set_conditions(Conditions { metered: true, ..Conditions::default() });
        w.wake();
        w.wait("closed by the hold", |w| w.live() == LiveChanges::Off && w.graph.sockets.open() == 0).await;
        let accepted = w.graph.sockets.accepted();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(w.graph.sockets.accepted(), accepted, "nothing is asked while held");
        w.running.set_conditions(Conditions::default());
        w.wake();
        w.connected().await;
        assert_eq!(w.cycles(), 0, "a stop's end is the poller's to act on (its own nudge)");
        w.stop().await;
    }
}
