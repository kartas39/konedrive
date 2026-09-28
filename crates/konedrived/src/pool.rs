//! The transfer pool: how many requests one account has in flight at once, downloads and
//! uploads together (issue #3, `docs/design/hydration.md` §6.4).
//!
//! OneDrive publishes no limit on concurrency; it throttles an account as a whole with `429`
//! or `503` and a `Retry-After`. So the pool finds its own level, per account:
//!
//! - it starts at [`START`] slots and **grows by one on every successful transfer** made while
//!   work is queued and every slot is busy — about doubling each round of transfers — up to the
//!   ceiling (`[transfers] max` in `config.toml`, [`DEFAULT_CEILING`]);
//! - **latency** (TCP Vegas style): each request's time to first byte is measured; while the
//!   median of the last [`LATENCY_WINDOW`] of a direction is more than [`SLOW_DOWN`] times the
//!   best of the last [`BASELINE_SPAN`], one slot is given back and the pool does not grow, until
//!   the median falls below [`RESUME`] times the baseline;
//! - a **`429`/`503`** on any request of the account halves the pool, once per burst, and no slot
//!   is handed out for the whole `Retry-After`. The size it came at is remembered for
//!   [`THROTTLE_MEMORY`]: at and above it the pool grows by one per round only.
//!
//! Who gets a free slot: a file being opened first — it may also take [`RESERVE`] slots above
//! the pool, and while any open waits or runs no background work takes a new slot; then
//! metadata operations; then background downloads and uploads, one to each in turn while both
//! have work. A pause holds back everything but opens.
//!
//! Every number here is a guess (the limitations log, section 5).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::Instant;

/// The pool's size when an account starts.
pub const START: usize = 16;
/// The ceiling when `config.toml` sets none.
pub const DEFAULT_CEILING: usize = 64;
/// The lowest and the highest ceiling `config.toml` may set; anything else is clamped.
pub const CEILING_MIN: usize = 1;
pub const CEILING_MAX: usize = 256;
/// Slots above the pool that only a file being opened may take.
pub const RESERVE: usize = 2;
/// How many of the last requests of a direction the median latency is taken over.
pub const LATENCY_WINDOW: usize = 20;
/// The fewest samples the latency is judged on.
pub const LATENCY_MIN_SAMPLES: usize = 5;
/// The baseline latency is the best of this long.
pub const BASELINE_SPAN: Duration = Duration::from_secs(300);
/// Over this many times the baseline, the pool backs off.
pub const SLOW_DOWN: f64 = 2.0;
/// Under this many times the baseline, it grows again.
pub const RESUME: f64 = 1.5;
/// How long the size a throttle came at is remembered.
pub const THROTTLE_MEMORY: Duration = Duration::from_secs(300);
/// A throttle this soon after the last one's wait ended belongs to the same burst.
pub const BURST_GRACE: Duration = Duration::from_secs(1);
/// How long the pool hands out nothing after a throttle that said no `Retry-After`.
pub const DEFAULT_THROTTLE_WAIT: Duration = Duration::from_secs(10);
/// The speed is the average of this long.
pub const SPEED_SPAN: Duration = Duration::from_secs(3);
/// How often the throughput is published while anything moves.
pub const PUBLISH_EVERY: Duration = Duration::from_secs(1);
/// The resolution of the speed meter.
const BUCKET: Duration = Duration::from_millis(250);
/// The resolution of the latency baseline.
const BASELINE_BUCKET: Duration = Duration::from_secs(5);

/// What a slot is taken for, in the order free slots go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A file being opened, or `Hydrate` ("download now").
    Open,
    /// `mkdir`, move, delete, and a move out of the folder.
    Metadata,
    /// Pinned files, replacements of changed files, thumbnails.
    Download,
    /// Content going up.
    Upload,
}

impl Class {
    fn index(self) -> usize {
        self as usize
    }
}

/// The direction of a request, for its latency and the bytes it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
}

/// What the pool publishes, once a second while anything moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Throughput {
    /// Bytes a second, the average of the last [`SPEED_SPAN`].
    pub down_speed: u64,
    pub up_speed: u64,
    /// Slots held by downloads (opens and background) and by uploads.
    pub active_down: u32,
    pub active_up: u32,
    pub size: u32,
    pub ceiling: u32,
}

type Observer = Arc<dyn Fn(Throughput) + Send + Sync>;

struct Waiter {
    id: u64,
    class: Class,
    granted: bool,
    waker: Option<Waker>,
}

/// One direction's latencies.
#[derive(Default)]
struct Latency {
    last: VecDeque<Duration>,
    /// The best of each [`BASELINE_BUCKET`], by when it began.
    best: VecDeque<(Instant, Duration)>,
}

impl Latency {
    fn add(&mut self, now: Instant, sample: Duration) {
        self.last.push_back(sample);
        while self.last.len() > LATENCY_WINDOW {
            self.last.pop_front();
        }
        match self.best.back_mut() {
            Some((at, best)) if now.duration_since(*at) < BASELINE_BUCKET => *best = (*best).min(sample),
            _ => self.best.push_back((now, sample)),
        }
        while self.best.front().is_some_and(|(at, _)| now.duration_since(*at) > BASELINE_SPAN) {
            self.best.pop_front();
        }
    }

    /// (median, baseline), once there are enough samples.
    fn judged(&self) -> Option<(Duration, Duration)> {
        if self.last.len() < LATENCY_MIN_SAMPLES {
            return None;
        }
        let mut sorted: Vec<Duration> = self.last.iter().copied().collect();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        let baseline = self.best.iter().map(|(_, d)| *d).min()?;
        Some((median, baseline))
    }
}

struct Inner {
    size: usize,
    ceiling: usize,
    /// Successes counted towards the next slot of slow growth.
    credit: usize,
    held: [usize; 4],
    waiters: VecDeque<Waiter>,
    next_id: u64,
    /// Whose turn it is when downloads and uploads both wait.
    turn: Direction,
    paused: bool,
    /// No slot is handed out before this (a throttle's `Retry-After`).
    blocked_until: Option<Instant>,
    /// A task waits for `blocked_until` to hand slots out again.
    unblock_armed: bool,
    /// The size the last throttle came at, and when.
    throttle_level: Option<(usize, Instant)>,
    /// The latency is above [`SLOW_DOWN`] times the baseline.
    congested: bool,
    latency: [Latency; 2],
    /// Bytes moved per [`BUCKET`], by the bucket's number since `epoch`.
    moved: VecDeque<(u64, [u64; 2])>,
    epoch: Instant,
    observer: Option<Observer>,
    publishing: bool,
}

/// One account's transfer pool. See the module's documentation.
pub struct TransferPool {
    inner: Mutex<Inner>,
    me: Weak<TransferPool>,
}

impl TransferPool {
    /// A pool that starts at [`START`] (or `ceiling`, if lower).
    pub fn new(ceiling: usize) -> Arc<Self> {
        Self::starting_at(START, ceiling)
    }

    /// A pool that starts at `start` slots (tests).
    pub fn starting_at(start: usize, ceiling: usize) -> Arc<Self> {
        let ceiling = ceiling.clamp(CEILING_MIN, CEILING_MAX);
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            inner: Mutex::new(Inner {
                size: start.clamp(1, ceiling),
                ceiling,
                credit: 0,
                held: [0; 4],
                waiters: VecDeque::new(),
                next_id: 0,
                turn: Direction::Down,
                paused: false,
                blocked_until: None,
                unblock_armed: false,
                throttle_level: None,
                congested: false,
                latency: [Latency::default(), Latency::default()],
                moved: VecDeque::new(),
                epoch: Instant::now(),
                observer: None,
                publishing: false,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The ceiling from now on (`config.toml`'s `[transfers] max`, already clamped); the pool
    /// shrinks to it if it is larger.
    pub fn set_ceiling(&self, ceiling: usize) {
        let mut inner = self.lock();
        inner.ceiling = ceiling.clamp(CEILING_MIN, CEILING_MAX);
        inner.size = inner.size.min(inner.ceiling);
        drop(inner);
        self.publish_now();
    }

    /// The pool's size now.
    pub fn size(&self) -> usize {
        self.lock().size
    }

    pub fn ceiling(&self) -> usize {
        self.lock().ceiling
    }

    /// Slots held now, by class.
    pub fn held(&self, class: Class) -> usize {
        self.lock().held[class.index()]
    }

    /// "Pause syncing": no new slot for anything but opens.
    pub fn set_paused(&self, paused: bool) {
        let wake = {
            let mut inner = self.lock();
            if inner.paused == paused {
                return;
            }
            inner.paused = paused;
            self.dispatch(&mut inner)
        };
        wake_all(wake);
    }

    /// Where the throughput goes: called at once with what it is now, then once a second
    /// while anything moves, and once more when it stops.
    pub fn set_observer(&self, observer: Observer) {
        self.lock().observer = Some(Arc::clone(&observer));
        observer(self.throughput());
    }

    /// Waits for a slot of `class`. Cancel-safe: a slot granted to a future dropped before
    /// it was polled again goes back.
    pub fn acquire(&self, class: Class) -> Acquire {
        Acquire { pool: self.arc(), class, id: None, done: false }
    }

    /// A slot of `class` if one would be handed out now, without waiting in line.
    pub fn try_acquire(&self, class: Class) -> Option<Slot> {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.waiters.push_back(Waiter { id, class, granted: false, waker: None });
        let wake = self.dispatch(&mut inner);
        let at = inner.waiters.iter().position(|w| w.id == id).expect("the waiter was just added");
        let granted = inner.waiters.remove(at).is_some_and(|w| w.granted);
        drop(inner);
        wake_all(wake);
        granted.then(|| self.slot(class))
    }

    fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("a pool is only used through its Arc")
    }

    fn slot(&self, class: Class) -> Slot {
        self.start_publishing();
        Slot { pool: self.arc(), class, succeeded: false }
    }

    /// A request of the account was answered `429` or `503`: the pool halves (once per
    /// burst), and hands out nothing for `wait` (`Retry-After`; [`DEFAULT_THROTTLE_WAIT`]
    /// when there was none).
    pub fn throttled(&self, wait: Option<Duration>) {
        let wait = wait.unwrap_or(DEFAULT_THROTTLE_WAIT);
        let now = Instant::now();
        let mut inner = self.lock();
        let same_burst = inner.blocked_until.is_some_and(|until| now < until + BURST_GRACE);
        let until = now + wait;
        inner.blocked_until = Some(inner.blocked_until.map_or(until, |b| b.max(until)));
        if !same_burst {
            let old = inner.size;
            inner.size = (old / 2).max(1);
            inner.credit = 0;
            inner.throttle_level = Some((old, now));
            tracing::warn!("transfer pool throttled: {old} -> {}", inner.size);
        } else if let Some((_, at)) = inner.throttle_level.as_mut() {
            *at = now;
        }
        self.arm_unblock(&mut inner);
    }

    /// A request's time to first byte (for an upload, from the end of its body).
    pub fn latency(&self, direction: Direction, sample: Duration) {
        let now = Instant::now();
        let mut inner = self.lock();
        inner.latency[direction as usize].add(now, sample);
        let judged: Vec<(Duration, Duration)> = inner.latency.iter().filter_map(Latency::judged).collect();
        let slow = judged.iter().any(|&(median, base)| median.as_secs_f64() > base.as_secs_f64() * SLOW_DOWN);
        let calm = judged.iter().all(|&(median, base)| median.as_secs_f64() < base.as_secs_f64() * RESUME);
        if slow && !inner.congested {
            inner.congested = true;
            inner.size = inner.size.saturating_sub(1).max(1);
            tracing::info!("transfer pool slowing down: latency is up, {} slot(s)", inner.size);
        } else if inner.congested && calm {
            inner.congested = false;
        }
    }

    /// `bytes` went `direction` just now.
    pub fn moved(&self, direction: Direction, bytes: u64) {
        if bytes == 0 {
            return;
        }
        {
            let mut inner = self.lock();
            let bucket = bucket_of(&inner, Instant::now());
            match inner.moved.back_mut() {
                Some((at, counts)) if *at == bucket => counts[direction as usize] += bytes,
                _ => {
                    let mut counts = [0; 2];
                    counts[direction as usize] = bytes;
                    inner.moved.push_back((bucket, counts));
                }
            }
            let keep = (SPEED_SPAN.as_millis() / BUCKET.as_millis()) as u64;
            while inner.moved.front().is_some_and(|(at, _)| at + keep <= bucket) {
                inner.moved.pop_front();
            }
        }
        self.start_publishing();
    }

    /// What the pool shows now.
    pub fn throughput(&self) -> Throughput {
        let inner = self.lock();
        throughput_of(&inner, Instant::now())
    }

    fn release(&self, class: Class, succeeded: bool) {
        let wake = {
            let mut inner = self.lock();
            if succeeded {
                grow(&mut inner);
            }
            inner.held[class.index()] -= 1;
            self.dispatch(&mut inner)
        };
        wake_all(wake);
    }

    /// Hands free slots to whoever is next, and says whom to wake.
    fn dispatch(&self, inner: &mut Inner) -> Vec<Waker> {
        let mut wake = Vec::new();
        if inner.blocked_until.is_some_and(|until| Instant::now() < until) {
            self.arm_unblock(inner);
            return wake;
        }
        loop {
            let total: usize = inner.held.iter().sum();
            let first = |inner: &Inner, class: Class| inner.waiters.iter().position(|w| !w.granted && w.class == class);
            if total < inner.size + RESERVE {
                if let Some(at) = first(inner, Class::Open) {
                    grant(inner, at, &mut wake);
                    continue;
                }
            }
            let opening = inner.held[Class::Open.index()] > 0 || first(inner, Class::Open).is_some();
            if opening || inner.paused || total >= inner.size {
                break;
            }
            if let Some(at) = first(inner, Class::Metadata) {
                grant(inner, at, &mut wake);
                continue;
            }
            let (down, up) = (first(inner, Class::Download), first(inner, Class::Upload));
            let (at, next) = match (down, up) {
                (Some(d), Some(u)) => match inner.turn {
                    Direction::Down => (d, Direction::Up),
                    Direction::Up => (u, Direction::Down),
                },
                (Some(d), None) => (d, Direction::Up),
                (None, Some(u)) => (u, Direction::Down),
                (None, None) => break,
            };
            inner.turn = next;
            grant(inner, at, &mut wake);
        }
        wake
    }

    /// A task that hands slots out again when a throttle's wait is over.
    fn arm_unblock(&self, inner: &mut Inner) {
        if inner.unblock_armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        inner.unblock_armed = true;
        let me = self.me.clone();
        runtime.spawn(async move {
            loop {
                let until = {
                    let Some(pool) = me.upgrade() else { return };
                    let until = pool.lock().blocked_until;
                    until
                };
                if let Some(until) = until.filter(|&u| Instant::now() < u) {
                    tokio::time::sleep_until(until).await;
                    continue;
                }
                let Some(pool) = me.upgrade() else { return };
                let wake = {
                    let mut inner = pool.lock();
                    inner.unblock_armed = false;
                    inner.blocked_until = None;
                    pool.dispatch(&mut inner)
                };
                wake_all(wake);
                return;
            }
        });
    }

    /// Starts the task that publishes the throughput once a second, if it is not running.
    fn start_publishing(&self) {
        let mut inner = self.lock();
        if inner.publishing || inner.observer.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        inner.publishing = true;
        let me = self.me.clone();
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(PUBLISH_EVERY).await;
                let Some(pool) = me.upgrade() else { return };
                let (shown, observer, idle) = {
                    let mut inner = pool.lock();
                    let shown = throughput_of(&inner, Instant::now());
                    let idle = inner.held.iter().sum::<usize>() == 0 && shown.down_speed == 0 && shown.up_speed == 0;
                    if idle {
                        inner.publishing = false;
                    }
                    (shown, inner.observer.clone(), idle)
                };
                if let Some(observer) = observer {
                    observer(shown);
                }
                if idle {
                    return;
                }
            }
        });
    }

    fn publish_now(&self) {
        let observer = self.lock().observer.clone();
        if let Some(observer) = observer {
            observer(self.throughput());
        }
    }
}

fn wake_all(wake: Vec<Waker>) {
    for waker in wake {
        waker.wake();
    }
}

fn grant(inner: &mut Inner, at: usize, wake: &mut Vec<Waker>) {
    let waiter = &mut inner.waiters[at];
    waiter.granted = true;
    let class = waiter.class;
    if let Some(waker) = waiter.waker.take() {
        wake.push(waker);
    }
    inner.held[class.index()] += 1;
}

/// One more slot after a success, when work waits and every slot is busy (the slot that
/// succeeded still counted as held).
fn grow(inner: &mut Inner) {
    let now = Instant::now();
    if inner.throttle_level.is_some_and(|(_, at)| now.duration_since(at) >= THROTTLE_MEMORY) {
        inner.throttle_level = None;
    }
    let busy = inner.held.iter().sum::<usize>() >= inner.size;
    let queued = inner.waiters.iter().any(|w| !w.granted);
    if !busy || !queued || inner.congested || inner.size >= inner.ceiling {
        return;
    }
    match inner.throttle_level {
        Some((level, _)) if inner.size >= level => {
            inner.credit += 1;
            if inner.credit >= inner.size {
                inner.credit = 0;
                inner.size += 1;
            }
        }
        _ => inner.size += 1,
    }
}

fn bucket_of(inner: &Inner, now: Instant) -> u64 {
    (now.duration_since(inner.epoch).as_millis() / BUCKET.as_millis()) as u64
}

fn throughput_of(inner: &Inner, now: Instant) -> Throughput {
    let bucket = bucket_of(inner, now);
    let keep = (SPEED_SPAN.as_millis() / BUCKET.as_millis()) as u64;
    let mut sums = [0u64; 2];
    for (at, counts) in &inner.moved {
        if at + keep > bucket {
            sums[0] += counts[0];
            sums[1] += counts[1];
        }
    }
    let per_second = |bytes: u64| (bytes as f64 / SPEED_SPAN.as_secs_f64()) as u64;
    Throughput {
        down_speed: per_second(sums[0]),
        up_speed: per_second(sums[1]),
        active_down: (inner.held[Class::Open.index()] + inner.held[Class::Download.index()]) as u32,
        active_up: inner.held[Class::Upload.index()] as u32,
        size: inner.size as u32,
        ceiling: inner.ceiling as u32,
    }
}

/// A slot held: given back when dropped. A transfer that went through says so first
/// ([`succeeded`](Slot::succeeded)), which is what makes the pool grow.
pub struct Slot {
    pool: Arc<TransferPool>,
    class: Class,
    succeeded: bool,
}

impl Slot {
    pub fn class(&self) -> Class {
        self.class
    }

    /// The transfer this slot was for went through.
    pub fn succeeded(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.pool.release(self.class, self.succeeded);
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").field("class", &self.class).finish()
    }
}

/// [`TransferPool::acquire`]'s future.
pub struct Acquire {
    pool: Arc<TransferPool>,
    class: Class,
    id: Option<u64>,
    done: bool,
}

impl Future for Acquire {
    type Output = Slot;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Slot> {
        let this = &mut *self;
        let (granted, wake) = {
            let mut inner = this.pool.lock();
            let mut wake = Vec::new();
            let id = match this.id {
                Some(id) => id,
                None => {
                    let id = inner.next_id;
                    inner.next_id += 1;
                    inner.waiters.push_back(Waiter { id, class: this.class, granted: false, waker: None });
                    this.id = Some(id);
                    wake = this.pool.dispatch(&mut inner);
                    id
                }
            };
            let at = inner.waiters.iter().position(|w| w.id == id).expect("a waiter stays until it is done");
            if inner.waiters[at].granted {
                inner.waiters.remove(at);
                (true, wake)
            } else {
                inner.waiters[at].waker = Some(cx.waker().clone());
                (false, wake)
            }
        };
        wake_all(wake);
        if granted {
            this.done = true;
            Poll::Ready(this.pool.slot(this.class))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        let Some(id) = self.id.filter(|_| !self.done) else { return };
        let wake = {
            let mut inner = self.pool.lock();
            let Some(at) = inner.waiters.iter().position(|w| w.id == id) else { return };
            let waiter = inner.waiters.remove(at).expect("found just now");
            if !waiter.granted {
                // Nothing was held; but a waiter gone may let another class go first.
                self.pool.dispatch(&mut inner)
            } else {
                inner.held[waiter.class.index()] -= 1;
                self.pool.dispatch(&mut inner)
            }
        };
        wake_all(wake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds `n` slots of `class`, taken without waiting.
    fn take(pool: &Arc<TransferPool>, class: Class, n: usize) -> Vec<Slot> {
        (0..n).map(|_| pool.try_acquire(class).expect("a free slot")).collect()
    }

    /// A waiter of `class`, polled once so that it stands in line.
    fn queue(pool: &Arc<TransferPool>, class: Class) -> Pin<Box<Acquire>> {
        let mut acquire = Box::pin(pool.acquire(class));
        let waker = futures_util::task::noop_waker();
        assert!(acquire.as_mut().poll(&mut Context::from_waker(&waker)).is_pending(), "{class:?} waits");
        acquire
    }

    fn ready(acquire: &mut Pin<Box<Acquire>>) -> Option<Slot> {
        let waker = futures_util::task::noop_waker();
        match acquire.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(slot) => Some(slot),
            Poll::Pending => None,
        }
    }

    /// One success, with work waiting and every slot busy, is one slot more; a success
    /// with nothing waiting is nothing.
    #[test]
    fn it_grows_on_a_success_only_while_work_waits_and_every_slot_is_busy() {
        let pool = TransferPool::starting_at(4, 64);
        let mut slots = take(&pool, Class::Download, 4);
        let _waiting = queue(&pool, Class::Download);
        let mut done = slots.pop().unwrap();
        done.succeeded();
        drop(done);
        assert_eq!(pool.size(), 5, "busy and work waiting: one more");

        let pool = TransferPool::starting_at(4, 64);
        let mut slots = take(&pool, Class::Download, 4);
        let mut done = slots.pop().unwrap();
        done.succeeded();
        drop(done);
        assert_eq!(pool.size(), 4, "nothing waits: idle gaps change nothing");

        let pool = TransferPool::starting_at(4, 64);
        let mut slots = take(&pool, Class::Download, 2);
        let mut done = slots.pop().unwrap();
        done.succeeded();
        drop(done);
        assert_eq!(pool.size(), 4, "slots free: nothing");

        let pool = TransferPool::starting_at(4, 4);
        let mut slots = take(&pool, Class::Download, 4);
        let _waiting = queue(&pool, Class::Download);
        let mut done = slots.pop().unwrap();
        done.succeeded();
        drop(done);
        assert_eq!(pool.size(), 4, "never above the ceiling");
    }

    /// A failure is no reason to grow.
    #[test]
    fn a_failed_transfer_does_not_grow_the_pool() {
        let pool = TransferPool::starting_at(2, 64);
        let mut slots = take(&pool, Class::Upload, 2);
        let _waiting = queue(&pool, Class::Upload);
        drop(slots.pop());
        assert_eq!(pool.size(), 2);
    }

    /// `429`: half the pool, nothing handed out for the wait, once per burst; then the level
    /// it came at is remembered: fast growth below it, one slot per round at and above it,
    /// and fast again once it is forgotten.
    #[tokio::test(start_paused = true)]
    async fn a_throttle_halves_the_pool_and_its_level_slows_growth_until_forgotten() {
        let pool = TransferPool::starting_at(8, 64);
        pool.throttled(Some(Duration::from_secs(5)));
        assert_eq!(pool.size(), 4);
        assert!(pool.try_acquire(Class::Open).is_none(), "nothing during Retry-After, not even an open");
        pool.throttled(Some(Duration::from_secs(5)));
        assert_eq!(pool.size(), 4, "the same burst halves once");
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(pool.try_acquire(Class::Download).is_some(), "slots again after the wait");

        // Below the level: one per success.
        let grow_once = |pool: &Arc<TransferPool>| {
            let size = pool.size();
            let mut slots = take(pool, Class::Download, size);
            let waiting = queue(pool, Class::Download);
            let mut done = slots.pop().unwrap();
            done.succeeded();
            drop(done);
            drop(waiting);
            drop(slots);
        };
        for _ in 0..4 {
            grow_once(&pool);
        }
        assert_eq!(pool.size(), 8, "fast up to the level the throttle came at");
        for _ in 0..7 {
            grow_once(&pool);
        }
        assert_eq!(pool.size(), 8, "at the level: not before a whole round");
        grow_once(&pool);
        assert_eq!(pool.size(), 9, "one slot after as many successes as slots");

        tokio::time::sleep(THROTTLE_MEMORY).await;
        grow_once(&pool);
        assert_eq!(pool.size(), 10, "forgotten after five minutes: fast again");
        grow_once(&pool);
        assert_eq!(pool.size(), 11);
    }

    /// A throttle with the pool at 1 leaves it at 1.
    #[tokio::test(start_paused = true)]
    async fn the_pool_never_halves_below_one() {
        let pool = TransferPool::starting_at(1, 64);
        pool.throttled(None);
        assert_eq!(pool.size(), 1);
    }

    /// Latency over twice the baseline gives one slot back and stops growth; it resumes
    /// only below one and a half times the baseline.
    #[tokio::test(start_paused = true)]
    async fn latency_backs_off_and_resumes_with_hysteresis() {
        let pool = TransferPool::starting_at(10, 64);
        for _ in 0..LATENCY_WINDOW {
            pool.latency(Direction::Down, Duration::from_millis(100));
        }
        assert_eq!(pool.size(), 10);
        for _ in 0..LATENCY_WINDOW {
            pool.latency(Direction::Down, Duration::from_millis(300));
        }
        assert_eq!(pool.size(), 9, "one slot back");

        let try_grow = |pool: &Arc<TransferPool>| {
            let size = pool.size();
            let mut slots = take(pool, Class::Download, size);
            let waiting = queue(pool, Class::Download);
            let mut done = slots.pop().unwrap();
            done.succeeded();
            drop((done, waiting, slots));
        };
        try_grow(&pool);
        assert_eq!(pool.size(), 9, "no growth while slow");

        for _ in 0..LATENCY_WINDOW {
            pool.latency(Direction::Down, Duration::from_millis(170));
        }
        try_grow(&pool);
        assert_eq!(pool.size(), 9, "between 1.5x and 2x: still no growth");

        for _ in 0..LATENCY_WINDOW {
            pool.latency(Direction::Down, Duration::from_millis(120));
        }
        try_grow(&pool);
        assert_eq!(pool.size(), 10, "below 1.5x: growth again");
    }

    /// An open goes first and may use the reserve above the pool; while one is under way,
    /// background work takes no new slot.
    #[test]
    fn an_open_goes_first_uses_the_reserve_and_holds_background_work_back() {
        let pool = TransferPool::starting_at(2, 64);
        let background = take(&pool, Class::Download, 2);
        let mut upload = queue(&pool, Class::Upload);
        let open = take(&pool, Class::Open, RESERVE);
        assert!(pool.try_acquire(Class::Open).is_none(), "the reserve is two slots");

        drop(background);
        assert!(ready(&mut upload).is_none(), "an open is under way: background work waits");
        let more = pool.try_acquire(Class::Open);
        assert!(more.is_some(), "freed slots go to opens");
        drop((open, more));
        assert!(ready(&mut upload).is_some(), "no open left: background work goes");
    }

    /// An open waiting in line holds background work back too, and gets the next slot.
    #[test]
    fn a_waiting_open_takes_the_next_free_slot() {
        let pool = TransferPool::starting_at(1, 64);
        let _open = take(&pool, Class::Open, 1 + RESERVE);
        let mut waiting_open = queue(&pool, Class::Open);
        let mut download = queue(&pool, Class::Download);
        drop(_open);
        let open = ready(&mut waiting_open);
        assert!(open.is_some());
        assert!(ready(&mut download).is_none(), "while the open runs");
        drop(open);
        assert!(ready(&mut download).is_some());
    }

    /// Downloads and uploads take turns while both wait; one alone takes every slot.
    #[test]
    fn downloads_and_uploads_alternate_while_both_wait() {
        let pool = TransferPool::starting_at(1, 64);
        let held = take(&pool, Class::Download, 1);
        let mut waiting: Vec<(Direction, Option<Pin<Box<Acquire>>>)> = (0..3)
            .map(|_| (Direction::Down, Some(queue(&pool, Class::Download))))
            .chain((0..3).map(|_| (Direction::Up, Some(queue(&pool, Class::Upload)))))
            .collect();
        drop(held);
        let mut order = Vec::new();
        for _ in 0..6 {
            let (direction, slot) = waiting
                .iter_mut()
                .find_map(|(direction, acquire)| {
                    let slot = ready(acquire.as_mut()?)?;
                    *acquire = None;
                    Some((*direction, slot))
                })
                .expect("one slot is handed out");
            order.push(direction);
            drop(slot);
        }
        assert_eq!(
            order,
            [Direction::Up, Direction::Down, Direction::Up, Direction::Down, Direction::Up, Direction::Down],
            "the slot held was a download's: uploads' turn first"
        );

        let pool = TransferPool::starting_at(3, 64);
        let alone = take(&pool, Class::Upload, 3);
        assert_eq!(alone.len(), 3, "only uploads: every slot");
    }

    /// Metadata goes before background transfers; a pause holds back everything but opens.
    #[test]
    fn metadata_goes_first_and_a_pause_holds_back_all_but_opens() {
        let pool = TransferPool::starting_at(1, 64);
        let held = take(&pool, Class::Upload, 1);
        let mut download = queue(&pool, Class::Download);
        let mut meta = queue(&pool, Class::Metadata);
        drop(held);
        assert!(ready(&mut download).is_none());
        assert!(ready(&mut meta).is_some());

        let pool = TransferPool::starting_at(4, 64);
        pool.set_paused(true);
        assert!(pool.try_acquire(Class::Upload).is_none());
        assert!(pool.try_acquire(Class::Metadata).is_none());
        assert!(pool.try_acquire(Class::Download).is_none());
        assert!(pool.try_acquire(Class::Open).is_some());
        pool.set_paused(false);
        assert!(pool.try_acquire(Class::Download).is_some());
    }

    /// A slot granted to a future dropped before it saw it goes back.
    #[test]
    fn a_granted_slot_of_a_dropped_waiter_goes_back() {
        let pool = TransferPool::starting_at(1, 64);
        let held = take(&pool, Class::Download, 1);
        let waiting = queue(&pool, Class::Download);
        drop(held);
        assert_eq!(pool.held(Class::Download), 1, "granted to the waiter");
        drop(waiting);
        assert_eq!(pool.held(Class::Download), 0);
    }

    /// The speed is the average of the last three seconds.
    #[tokio::test(start_paused = true)]
    async fn the_speed_is_the_average_of_three_seconds() {
        let pool = TransferPool::starting_at(4, 64);
        pool.moved(Direction::Down, 3_000_000);
        pool.moved(Direction::Up, 300);
        let shown = pool.throughput();
        assert_eq!((shown.down_speed, shown.up_speed), (1_000_000, 100));
        tokio::time::sleep(SPEED_SPAN + BUCKET).await;
        assert_eq!(pool.throughput().down_speed, 0, "decays to 0");
    }
}
