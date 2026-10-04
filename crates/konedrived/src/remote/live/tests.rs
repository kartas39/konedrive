use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::*;
use crate::conditions::running::Conditions;
use crate::fake_onedrive::{Early, ROOT};
use crate::remote::testing::World;

const DEBOUNCE: Duration = Duration::from_millis(300);

/// One account's live task against the fixture's fake OneDrive, and the cycles it asked for.
struct Lived {
    world: World,
    running: Arc<Running>,
    up: Arc<watch::Sender<bool>>,
    cycles: Arc<AtomicUsize>,
    cancel: CancellationToken,
    live: Option<Live>,
    files: usize,
}

impl std::ops::Deref for Lived {
    type Target = World;

    fn deref(&self) -> &World {
        &self.world
    }
}

async fn world() -> Lived {
    world_with(|_| {}).await
}

async fn world_with(setup: impl FnOnce(&mut crate::fake_onedrive::Cloud)) -> Lived {
    world_timed(setup, |_| {}).await
}

async fn world_timed(setup: impl FnOnce(&mut crate::fake_onedrive::Cloud), adjust: impl FnOnce(&mut Timing)) -> Lived {
    let world = World::read_only().await;
    world.graph.with(setup);
    let running = Arc::new(Running::default());
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
        drive: world.drive(),
        store: world.store.clone(),
        running: Arc::clone(&running),
        state: world.state.clone(),
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
    Lived { world, running, up, cycles, cancel, live, files: 0 }
}

impl Lived {
    fn live(&self) -> LiveChanges {
        self.state.get().cycle.live_changes
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

    crate::conditions::running::set_paused(&w.store, Some(0)).await.unwrap();
    w.wake();
    w.wait("closed by the pause", |w| w.live() == LiveChanges::Off && w.graph.sockets.open() == 0 && !*w.up.borrow()).await;
    let (endpoints, accepted) = (w.endpoints(), w.graph.sockets.accepted());
    w.wake();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!((w.endpoints(), w.graph.sockets.accepted()), (endpoints, accepted), "nothing is asked while paused");
    crate::conditions::running::set_paused(&w.store, None).await.unwrap();
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
