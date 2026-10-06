use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use super::{PauseClock, LOOK};
use crate::conditions::running::Clock;

/// The time the scene starts at, in unix seconds.
const START: i64 = 1_700_000_000;

/// A clock with what the service gives it: a kept pause (the part of the account's settings), a place
/// the pause is shown (the bus's part), and a wall clock that follows tokio's paused time,
/// which a test can push ahead as a suspend does. The keeper of the pause reads that same
/// clock, as the service's does.
struct Scene {
    clock: Option<Arc<PauseClock>>,
    parts: Arc<Parts>,
}

/// A wall clock that follows tokio's paused time, `ahead` of it by what a test says; its
/// sleeps are tokio's, which do not count a suspend.
struct Wall {
    started: tokio::time::Instant,
    ahead: AtomicI64,
}

impl Clock for Wall {
    fn now(&self) -> i64 {
        START + self.started.elapsed().as_secs() as i64 + self.ahead.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, at: i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(Duration::from_secs((at - self.now()).max(0) as u64)))
    }
}

struct Parts {
    wall: Arc<Wall>,
    kept: Mutex<Option<i64>>,
    /// Every time the pause was shown: when, and what.
    shown: Mutex<Vec<(i64, Option<i64>)>>,
    /// How many times the clock said that a timed pause has run out.
    run_out: AtomicUsize,
    /// A `Pause` of so many seconds that is written the next time the clock says so,
    /// before that is shown.
    lands: Mutex<Option<i64>>,
}

impl Parts {
    /// What `SyncService::show_pause` does: the kept pause, less a timed one that is over,
    /// is shown.
    fn show(&self, clock: &PauseClock) {
        clock.show(|| {
            let now = self.wall.now();
            let mut kept = self.kept.lock().unwrap();
            if kept.is_some_and(|until| until > 0 && until <= now) {
                *kept = None;
            }
            self.shown.lock().unwrap().push((now - START, *kept));
            *kept
        });
    }

    fn keep(&self, seconds: i64) {
        *self.kept.lock().unwrap() = Some(if seconds == 0 { 0 } else { self.wall.now() + seconds });
    }
}

impl Scene {
    fn new() -> Self {
        let wall = Arc::new(Wall { started: tokio::time::Instant::now(), ahead: AtomicI64::new(0) });
        let parts = Arc::new(Parts {
            wall: Arc::clone(&wall),
            kept: Mutex::new(None),
            shown: Mutex::new(Vec::new()),
            run_out: AtomicUsize::new(0),
            lands: Mutex::new(None),
        });
        let clock = Arc::new_cyclic(|me: &Weak<PauseClock>| {
            let (me, parts) = (me.clone(), Arc::clone(&parts));
            PauseClock::new(wall, move || {
                parts.run_out.fetch_add(1, Ordering::SeqCst);
                if let Some(seconds) = parts.lands.lock().unwrap().take() {
                    parts.keep(seconds);
                }
                if let Some(clock) = me.upgrade() {
                    parts.show(&clock);
                }
            })
        });
        Self { clock: Some(clock), parts }
    }

    fn clock(&self) -> &PauseClock {
        self.clock.as_ref().unwrap()
    }

    /// `Pause(seconds)`.
    fn pause(&self, seconds: i64) {
        self.parts.keep(seconds);
        self.parts.show(self.clock());
    }

    /// `Resume()`.
    fn resume(&self) {
        *self.parts.kept.lock().unwrap() = None;
        self.parts.show(self.clock());
    }

    /// The pause as it is shown now, in seconds from the start.
    fn shown(&self) -> Option<i64> {
        self.parts.shown.lock().unwrap().last().and_then(|(_, pause)| *pause).map(|until| if until == 0 { 0 } else { until - START })
    }

    /// When the pause was last shown, in seconds from the start.
    fn last_shown_at(&self) -> i64 {
        self.parts.shown.lock().unwrap().last().map_or(-1, |(at, _)| *at)
    }

    fn run_out(&self) -> usize {
        self.parts.run_out.load(Ordering::SeqCst)
    }
}

async fn pass(seconds: u64) {
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

/// A timed pause is shown until its time and ends by itself then, not before; a pause
/// until resumed is never ended by the clock, and one resumed early is not ended again.
#[tokio::test(start_paused = true)]
async fn a_timed_pause_ends_by_itself_and_one_until_resumed_only_by_resume() {
    let scene = Scene::new();
    scene.pause(3600);
    assert_eq!(scene.shown(), Some(3600));
    pass(3599).await;
    assert_eq!((scene.shown(), scene.run_out()), (Some(3600), 0), "not before its time");
    pass(2).await;
    assert_eq!((scene.shown(), scene.last_shown_at(), scene.run_out()), (None, 3600, 1), "ended at its time, once");

    scene.pause(0);
    pass(86_400).await;
    assert_eq!((scene.shown(), scene.run_out()), (Some(0), 1), "until resumed");
    scene.resume();
    assert_eq!(scene.shown(), None);

    scene.pause(600);
    pass(10).await;
    scene.resume();
    pass(1200).await;
    assert_eq!((scene.shown(), scene.run_out()), (None, 1), "resumed early: nothing left to end");
}

/// A `Pause` written just as the clock finds the one before it over is the one shown, and
/// is ended at its own time.
#[tokio::test(start_paused = true)]
async fn a_pause_that_lands_as_the_last_one_ends_stands() {
    let scene = Scene::new();
    scene.pause(60);
    *scene.parts.lands.lock().unwrap() = Some(7200);
    pass(61).await;
    assert_eq!((scene.shown(), scene.run_out()), (Some(60 + 7200), 1), "the new pause is shown, not the end of the old");
    let never_resumed = scene.parts.shown.lock().unwrap().iter().all(|(_, pause)| pause.is_some());
    assert!(never_resumed, "and the account never read as running in between");
    pass(7200).await;
    assert_eq!((scene.shown(), scene.last_shown_at(), scene.run_out()), (None, 60 + 7200, 2), "ended at its own time");
}

/// The machine slept: the wall clock is ahead of what the timer's sleep counted. The pause
/// ends at the timer's next look, at most a minute later, not after the sleep's full time.
#[tokio::test(start_paused = true)]
async fn a_suspend_does_not_stretch_a_timed_pause() {
    let scene = Scene::new();
    scene.pause(3600);
    pass(90).await;
    scene.parts.wall.ahead.store(4000, Ordering::SeqCst);
    pass(LOOK as u64).await;
    assert_eq!((scene.shown(), scene.run_out()), (None, 1));
}

/// A shorter pause set while the timer sleeps for a longer one ends at its own time, not at
/// the timer's next look.
#[tokio::test(start_paused = true)]
async fn a_shorter_pause_set_over_a_longer_one_ends_at_its_own_time() {
    let scene = Scene::new();
    scene.pause(3600);
    pass(5).await;
    scene.pause(10);
    pass(11).await;
    assert_eq!((scene.shown(), scene.last_shown_at(), scene.run_out()), (None, 15, 1));
}

/// A forgotten pause is taken off at once and never ended by the timer; the next pause is
/// timed again; and a clock that is dropped says nothing more.
#[tokio::test(start_paused = true)]
async fn a_forgotten_pause_is_not_ended_later_and_the_next_one_is() {
    let mut scene = Scene::new();
    scene.pause(600);
    let parts = Arc::clone(&scene.parts);
    scene.clock().forget(|| {
        *parts.kept.lock().unwrap() = None;
        parts.shown.lock().unwrap().push((0, None));
    });
    assert_eq!(scene.shown(), None);
    pass(1200).await;
    assert_eq!((scene.run_out(), scene.last_shown_at()), (0, 0), "the timer went with the pause");

    scene.pause(600);
    pass(601).await;
    assert_eq!((scene.shown(), scene.run_out()), (None, 1), "a pause after a Forget ends by itself");

    scene.pause(600);
    scene.clock = None;
    pass(1200).await;
    assert_eq!(scene.run_out(), 1, "the timer went with the clock");
}
