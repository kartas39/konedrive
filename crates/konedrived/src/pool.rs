//! The transfer pool: how many requests one account has in flight at once, downloads and
//! uploads together (issue #3, `docs/design/hydration.md` §6.4).
//!
//! OneDrive publishes no limit on concurrency; it throttles an account as a whole with `429`
//! or `503` and a `Retry-After`. So the pool finds its own level, per account:
//!
//! - it starts at [`START`] slots and **grows by one on every successful transfer** made while
//!   work is queued and every slot is busy — about doubling each round of transfers — up to the
//!   ceiling (`[transfers] max` in `config.toml`, [`DEFAULT_CEILING`]). Latency is not measured:
//!   only a throttle stops the growth;
//! - a **`429`/`503`** on any request of the account halves the pool, once per burst, and no slot
//!   is handed out for the whole `Retry-After`. The size it came at is remembered for
//!   [`THROTTLE_MEMORY`]: at and above it the pool grows by one per round only.
//!
//! A **large** sync transfer (a file of [`LARGE_FROM`] or more, of any class but
//! [`Class::Open`]) fills the link on its own: `[transfers] large` ([`DEFAULT_LARGE`]) limits the
//! streams of large sync transfers that run at once, each in a slot of the pool; a large one
//! waiting for that limit lets the small ones behind it go. A file being opened is outside the
//! limit and its count (issue #50): it is never marked large, neither waits for the limit nor
//! takes room in it, and still takes a slot of the pool. A large pinned download in parts holds
//! one large slot per stream (`sync::source::parts`, issue #28): its extra streams take only
//! slots nothing waits for ([`TransferPool::waiting`]), and give them back when something does.
//!
//! Who gets a free slot: a file being opened first — it may also take [`RESERVE`] slots above
//! the pool, and while any open waits or runs no background work takes a new slot; then
//! metadata operations; then background downloads and uploads, one to each in turn while both
//! have work. A pause holds back everything but opens.
//!
//! Every number here is a guess (the limitations log, section 5).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::Instant;

/// The pool's size when an account starts.
pub const START: usize = 16;
/// The ceiling when `config.toml` sets none.
pub const DEFAULT_CEILING: usize = 32;
/// The lowest and the highest ceiling `config.toml` may set; anything else is clamped.
pub const CEILING_MIN: usize = 1;
pub const CEILING_MAX: usize = 256;
/// A file is large from this size up (bytes).
pub const LARGE_FROM: u64 = 100 * 1024 * 1024;
/// Large transfers at once when `config.toml` sets none (`[transfers] large`).
pub const DEFAULT_LARGE: usize = 4;
/// Slots above the pool that only a file being opened may take.
pub const RESERVE: usize = 2;
/// How long the size a throttle came at is remembered.
pub const THROTTLE_MEMORY: Duration = Duration::from_secs(300);
/// A throttle this soon after the last one's wait ended belongs to the same burst.
pub const BURST_GRACE: Duration = Duration::from_secs(1);
/// How long the pool hands out nothing after a throttle that said no `Retry-After`.
pub const DEFAULT_THROTTLE_WAIT: Duration = Duration::from_secs(10);
/// The speed is the average of this long.
pub const SPEED_SPAN: Duration = Duration::from_secs(3);
/// The speed a queue's time left is worked out from is the average of this long (issue #16):
/// longer than [`SPEED_SPAN`], so that the estimate does not jump with every burst. A guess.
pub const AVERAGE_SPAN: Duration = Duration::from_secs(30);
/// A direction in which nothing has moved for this long has no average speed, and so no time
/// left (issue #16). A guess.
pub const STILL_AFTER: Duration = Duration::from_secs(10);
/// The shortest span the average is taken over at the start of a run, so that the first
/// bytes of a run do not make a speed of their own.
const AVERAGE_FLOOR: Duration = Duration::from_secs(1);
/// How often the throughput is published while anything moves.
pub const PUBLISH_EVERY: Duration = Duration::from_secs(1);
/// The resolution of the speed meter.
const BUCKET: Duration = Duration::from_millis(250);

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

/// Whether a transfer is large ([`LARGE_FROM`] bytes or more): at most `[transfers] large` of
/// those run at once, openings aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    Large,
}

impl Size {
    /// The size class of a file of `bytes`: its placeholder's size, or the local file's.
    pub fn of(bytes: u64) -> Self {
        if bytes >= LARGE_FROM {
            Size::Large
        } else {
            Size::Small
        }
    }
}

/// The direction of the bytes a request moves.
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
    /// Every slot held now, of all four classes, the opens' reserve included: may be above
    /// `size` (issue #50).
    pub in_use: u32,
    pub size: u32,
    pub ceiling: u32,
    /// The streams of large sync transfers under way (each stream of a download in parts; never
    /// a file being opened), and how many may run at once.
    pub large: u32,
    pub large_limit: u32,
    /// Seconds left of OneDrive's `Retry-After`, during which no slot is handed out; 0 when
    /// there is none.
    pub retry_after: u32,
    /// Bytes moved each way since the pool started (issue #16: what a run has done).
    pub down_moved: u64,
    pub up_moved: u64,
    /// Bytes a second each way, the average of the last [`AVERAGE_SPAN`] (or of the run so
    /// far, when it began within it); 0 once nothing has moved that way for [`STILL_AFTER`]
    /// (issue #16: what a queue's time left is worked out from).
    pub down_average: u64,
    pub up_average: u64,
}

type Observer = Arc<dyn Fn(Throughput) + Send + Sync>;

struct Waiter {
    class: Class,
    large: bool,
    granted: bool,
    waker: Option<Waker>,
}

/// Who waits for a slot (issue #39): every waiter by its id, and those not
/// granted yet in line by class and size, in the order they came (ids
/// only grow). A grant, a release and a waiter that gives up find their
/// waiter without a search.
#[derive(Default)]
struct Waiters {
    by_id: HashMap<u64, Waiter>,
    /// `[class][large]`.
    line: [[BTreeSet<u64>; 2]; 4],
}

impl Waiters {
    fn add(&mut self, id: u64, class: Class, large: bool) {
        self.by_id.insert(id, Waiter { class, large, granted: false, waker: None });
        self.line[class.index()][usize::from(large)].insert(id);
    }

    /// Takes waiter `id` out, whether granted or not.
    fn remove(&mut self, id: u64) -> Option<Waiter> {
        let waiter = self.by_id.remove(&id)?;
        if !waiter.granted {
            self.line[waiter.class.index()][usize::from(waiter.large)].remove(&id);
        }
        Some(waiter)
    }

    /// Whether any waiter has not been granted a slot yet.
    fn queued(&self) -> bool {
        self.line.iter().flatten().any(|line| !line.is_empty())
    }
}

struct Inner {
    size: usize,
    ceiling: usize,
    /// Streams of large sync transfers at once; an open is never one.
    large_limit: usize,
    /// Successes counted towards the next slot of slow growth.
    credit: usize,
    held: [usize; 4],
    /// Of those, the streams of large sync transfers.
    large_held: usize,
    waiters: Waiters,
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
    /// Bytes moved per [`BUCKET`], by the bucket's number since `epoch`, for the last
    /// [`AVERAGE_SPAN`].
    moved: VecDeque<(u64, [u64; 2])>,
    /// Bytes moved each way since the pool started.
    moved_total: [u64; 2],
    epoch: Instant,
    observer: Option<Observer>,
    publishing: bool,
}

impl Inner {
    fn blocked(&self, now: Instant) -> bool {
        self.blocked_until.is_some_and(|until| now < until)
    }
}

/// One account's transfer pool. See the module's documentation.
pub struct TransferPool {
    inner: Mutex<Inner>,
    me: Weak<TransferPool>,
}

impl TransferPool {
    /// A pool that starts at [`START`] (or `ceiling`, if lower), with [`DEFAULT_LARGE`] large
    /// transfers at once.
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
                large_limit: DEFAULT_LARGE.clamp(1, ceiling),
                credit: 0,
                held: [0; 4],
                large_held: 0,
                waiters: Waiters::default(),
                next_id: 0,
                turn: Direction::Down,
                paused: false,
                blocked_until: None,
                unblock_armed: false,
                throttle_level: None,
                moved: VecDeque::new(),
                moved_total: [0; 2],
                epoch: Instant::now(),
                observer: None,
                publishing: false,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The limits from now on (`config.toml`'s `[transfers] max` and `large`, already
    /// clamped): the pool shrinks to the ceiling if it is larger, and the large-file limit
    /// is kept within 1 and the ceiling.
    pub fn set_limits(&self, ceiling: usize, large: usize) {
        let wake = {
            let mut inner = self.lock();
            inner.ceiling = ceiling.clamp(CEILING_MIN, CEILING_MAX);
            inner.size = inner.size.min(inner.ceiling);
            inner.large_limit = large.clamp(1, inner.ceiling);
            self.dispatch(&mut inner)
        };
        wake_all(wake);
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

    /// Streams of large sync transfers under way now; a file being opened is never one.
    pub fn large_held(&self) -> usize {
        self.lock().large_held
    }

    /// Whether anything waits for a slot now: a transfer in line that has not been handed
    /// one. An extra stream of a download in parts takes only a slot nothing waits for, and
    /// gives its slot back when something does (issue #28).
    pub fn waiting(&self) -> bool {
        self.lock().waiters.queued()
    }

    /// "Pause syncing": no new slot for anything but opens. Whether it was paused before.
    pub fn set_paused(&self, paused: bool) -> bool {
        let wake = {
            let mut inner = self.lock();
            if inner.paused == paused {
                return paused;
            }
            inner.paused = paused;
            self.dispatch(&mut inner)
        };
        wake_all(wake);
        !paused
    }

    /// Where the throughput goes: called at once with what it is now, then once a second
    /// while anything moves (or a `Retry-After` runs), and once more when it stops.
    pub fn set_observer(&self, observer: Observer) {
        self.lock().observer = Some(Arc::clone(&observer));
        observer(self.throughput());
    }

    /// Waits for a small slot of `class`. Cancel-safe: a slot granted to a future dropped
    /// before it was polled again goes back.
    pub fn acquire(&self, class: Class) -> Acquire {
        self.acquire_sized(class, Size::Small)
    }

    /// Waits for a slot of `class` for a transfer of `size`: a large sync transfer also waits
    /// for the large-stream limit. An open is never large ([`is_large`]).
    pub fn acquire_sized(&self, class: Class, size: Size) -> Acquire {
        Acquire { pool: self.arc(), class, large: is_large(class, size), id: None, done: false }
    }

    /// A small slot of `class` if one would be handed out now, without waiting in line.
    pub fn try_acquire(&self, class: Class) -> Option<Slot> {
        self.try_acquire_sized(class, Size::Small)
    }

    /// A slot of `class` for a transfer of `size`, if one would be handed out now.
    pub fn try_acquire_sized(&self, class: Class, size: Size) -> Option<Slot> {
        let large = is_large(class, size);
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.waiters.add(id, class, large);
        let wake = self.dispatch(&mut inner);
        let granted = inner.waiters.remove(id).is_some_and(|w| w.granted);
        drop(inner);
        wake_all(wake);
        granted.then(|| self.slot(class, large))
    }

    fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("a pool is only used through its Arc")
    }

    fn slot(&self, class: Class, large: bool) -> Slot {
        self.start_publishing();
        Slot { pool: self.arc(), class, large, succeeded: false }
    }

    /// A request of the account was answered `429` or `503`: the pool halves (once per
    /// burst), and hands out nothing for `wait` (`Retry-After`; [`DEFAULT_THROTTLE_WAIT`]
    /// when there was none).
    pub fn throttled(&self, wait: Option<Duration>) {
        let wait = wait.unwrap_or(DEFAULT_THROTTLE_WAIT);
        let now = Instant::now();
        {
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
        // The wait counts down on the bus, whether or not anything moves.
        self.publish_now();
        self.start_publishing();
    }

    /// `bytes` went `direction` just now.
    pub fn moved(&self, direction: Direction, bytes: u64) {
        if bytes == 0 {
            return;
        }
        {
            let mut inner = self.lock();
            inner.moved_total[direction as usize] += bytes;
            let bucket = bucket_of(&inner, Instant::now());
            match inner.moved.back_mut() {
                Some((at, counts)) if *at == bucket => counts[direction as usize] += bytes,
                _ => {
                    let mut counts = [0; 2];
                    counts[direction as usize] = bytes;
                    inner.moved.push_back((bucket, counts));
                }
            }
            let keep = (AVERAGE_SPAN.as_millis() / BUCKET.as_millis()) as u64;
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

    fn release(&self, class: Class, large: bool, succeeded: bool) {
        let wake = {
            let mut inner = self.lock();
            if succeeded {
                grow(&mut inner);
            }
            inner.held[class.index()] -= 1;
            if large {
                inner.large_held -= 1;
            }
            self.dispatch(&mut inner)
        };
        wake_all(wake);
    }

    /// Hands free slots to whoever is next, and says whom to wake.
    fn dispatch(&self, inner: &mut Inner) -> Vec<Waker> {
        let mut wake = Vec::new();
        if inner.blocked(Instant::now()) {
            self.arm_unblock(inner);
            return wake;
        }
        loop {
            let total: usize = inner.held.iter().sum();
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
                pool.publish_now();
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
                    let now = Instant::now();
                    let shown = throughput_of(&inner, now);
                    // Until nothing has moved for `STILL_AFTER`: the average, and with it
                    // the time left, goes to 0 on the bus too.
                    let idle = inner.held.iter().sum::<usize>() == 0
                        && shown.down_speed == 0
                        && shown.up_speed == 0
                        && shown.down_average == 0
                        && shown.up_average == 0
                        && !inner.blocked(now);
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

/// Whether a transfer of `class` and `size` counts against the large-stream limit: a large
/// one of any class but [`Class::Open`]. A file being opened is outside the limit and its count.
fn is_large(class: Class, size: Size) -> bool {
    size == Size::Large && class != Class::Open
}

fn wake_all(wake: Vec<Waker>) {
    for waker in wake {
        waker.wake();
    }
}

/// The first waiter of `class` that may have a slot: a large one only while the large-stream
/// limit leaves room (an open is never large).
fn first(inner: &Inner, class: Class) -> Option<u64> {
    let large_free = inner.large_held < inner.large_limit;
    let [small, large] = &inner.waiters.line[class.index()];
    let small = small.first().copied();
    let large = large.first().copied().filter(|_| large_free);
    match (small, large) {
        (Some(s), Some(l)) => Some(s.min(l)),
        (s, l) => s.or(l),
    }
}

fn grant(inner: &mut Inner, id: u64, wake: &mut Vec<Waker>) {
    let waiter = inner.waiters.by_id.get_mut(&id).expect("a waiter in line is known by its id");
    waiter.granted = true;
    let (class, large) = (waiter.class, waiter.large);
    if let Some(waker) = waiter.waker.take() {
        wake.push(waker);
    }
    inner.waiters.line[class.index()][usize::from(large)].remove(&id);
    inner.held[class.index()] += 1;
    if large {
        inner.large_held += 1;
    }
}

/// One more slot after a success, when work waits and every slot is busy (the slot that
/// succeeded still counted as held).
fn grow(inner: &mut Inner) {
    let now = Instant::now();
    if inner.throttle_level.is_some_and(|(_, at)| now.duration_since(at) >= THROTTLE_MEMORY) {
        inner.throttle_level = None;
    }
    let busy = inner.held.iter().sum::<usize>() >= inner.size;
    let queued = inner.waiters.queued();
    if !busy || !queued || inner.size >= inner.ceiling {
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
    let average = |direction: usize| average_of(inner, now, direction);
    // Whole seconds left, rounded up: "wait 30 s" until the very end.
    let retry_after = inner.blocked_until.map_or(0, |until| {
        let left = until.saturating_duration_since(now);
        left.as_secs() + u64::from(left.subsec_nanos() > 0)
    });
    Throughput {
        down_speed: per_second(sums[0]),
        up_speed: per_second(sums[1]),
        in_use: inner.held.iter().sum::<usize>() as u32,
        size: inner.size as u32,
        ceiling: inner.ceiling as u32,
        large: inner.large_held as u32,
        large_limit: inner.large_limit as u32,
        retry_after: u32::try_from(retry_after).unwrap_or(u32::MAX),
        down_moved: inner.moved_total[0],
        up_moved: inner.moved_total[1],
        down_average: average(0),
        up_average: average(1),
    }
}

/// Bytes a second moved `direction` (the index of a [`Direction`]) over the last
/// [`AVERAGE_SPAN`] — or since the first bucket within it that moved any, when that is
/// later, but never less than [`AVERAGE_FLOOR`]; 0 when nothing has moved that way for
/// [`STILL_AFTER`].
fn average_of(inner: &Inner, now: Instant, direction: usize) -> u64 {
    let bucket = bucket_of(inner, now);
    let keep = (AVERAGE_SPAN.as_millis() / BUCKET.as_millis()) as u64;
    let still = (STILL_AFTER.as_millis() / BUCKET.as_millis()) as u64;
    let (mut first, mut last, mut sum) = (None, 0, 0u64);
    for (at, counts) in &inner.moved {
        if at + keep > bucket && counts[direction] > 0 {
            first.get_or_insert(*at);
            last = *at;
            sum += counts[direction];
        }
    }
    let Some(first) = first else { return 0 };
    if last + still <= bucket {
        return 0;
    }
    let began = inner.epoch + BUCKET * u32::try_from(first).unwrap_or(u32::MAX);
    let span = now.saturating_duration_since(began).clamp(AVERAGE_FLOOR, AVERAGE_SPAN);
    (sum as f64 / span.as_secs_f64()) as u64
}

/// A slot held: given back when dropped. A transfer that went through says so first
/// ([`succeeded`](Slot::succeeded)), which is what makes the pool grow.
pub struct Slot {
    pool: Arc<TransferPool>,
    class: Class,
    large: bool,
    succeeded: bool,
}

impl Slot {
    pub fn class(&self) -> Class {
        self.class
    }

    /// The size class it was taken for.
    pub fn size(&self) -> Size {
        if self.large {
            Size::Large
        } else {
            Size::Small
        }
    }

    /// The transfer this slot was for went through.
    pub fn succeeded(&mut self) {
        self.succeeded = true;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.pool.release(self.class, self.large, self.succeeded);
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").field("class", &self.class).field("large", &self.large).finish()
    }
}

/// [`TransferPool::acquire`]'s future.
pub struct Acquire {
    pool: Arc<TransferPool>,
    class: Class,
    large: bool,
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
                    inner.waiters.add(id, this.class, this.large);
                    this.id = Some(id);
                    wake = this.pool.dispatch(&mut inner);
                    id
                }
            };
            let waiter = inner.waiters.by_id.get_mut(&id).expect("a waiter stays until it is done");
            if waiter.granted {
                inner.waiters.remove(id);
                (true, wake)
            } else {
                waiter.waker = Some(cx.waker().clone());
                (false, wake)
            }
        };
        wake_all(wake);
        if granted {
            this.done = true;
            Poll::Ready(this.pool.slot(this.class, this.large))
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
            let Some(waiter) = inner.waiters.remove(id) else { return };
            if waiter.granted {
                inner.held[waiter.class.index()] -= 1;
                if waiter.large {
                    inner.large_held -= 1;
                }
            }
            // A slot back, or a waiter gone that may have let another class go first.
            self.pool.dispatch(&mut inner)
        };
        wake_all(wake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holds `n` slots of `class`, taken without waiting.
    fn take(pool: &Arc<TransferPool>, class: Class, n: usize) -> Vec<Slot> {
        take_sized(pool, class, Size::Small, n)
    }

    fn take_sized(pool: &Arc<TransferPool>, class: Class, size: Size, n: usize) -> Vec<Slot> {
        (0..n).map(|_| pool.try_acquire_sized(class, size).expect("a free slot")).collect()
    }

    /// A waiter of `class`, polled once so that it stands in line.
    fn queue(pool: &Arc<TransferPool>, class: Class) -> Pin<Box<Acquire>> {
        queue_sized(pool, class, Size::Small)
    }

    fn queue_sized(pool: &Arc<TransferPool>, class: Class, size: Size) -> Pin<Box<Acquire>> {
        let mut acquire = Box::pin(pool.acquire_sized(class, size));
        let waker = futures_util::task::noop_waker();
        assert!(acquire.as_mut().poll(&mut Context::from_waker(&waker)).is_pending(), "{class:?} {size:?} waits");
        acquire
    }

    fn ready(acquire: &mut Pin<Box<Acquire>>) -> Option<Slot> {
        let waker = futures_util::task::noop_waker();
        match acquire.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(slot) => Some(slot),
            Poll::Pending => None,
        }
    }

    /// Issue #39: among 30 000 waiters the pool grants in the order they came, and one in
    /// the middle that gives up leaves the line as it was around it.
    #[test]
    fn thirty_thousand_waiters_are_granted_in_order_and_one_gives_up() {
        let pool = TransferPool::starting_at(1, 1);
        let mut held = take(&pool, Class::Download, 1);
        let mut waiting: Vec<Option<Pin<Box<Acquire>>>> = (0..30_000).map(|_| Some(queue(&pool, Class::Download))).collect();
        drop(waiting[15_000].take());
        for n in (0..30_000).filter(|&n| n != 15_000) {
            drop(held.pop());
            for later in [n + 1, 29_999].into_iter().filter(|&l| l != 15_000 && l > n && l < 30_000) {
                assert!(ready(waiting[later].as_mut().unwrap()).is_none(), "{later} waits behind {n}");
            }
            let slot = ready(waiting[n].as_mut().unwrap()).unwrap_or_else(|| panic!("{n} is next"));
            held.push(slot);
            waiting[n] = None;
        }
        assert!(!pool.waiting());
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
        assert_eq!(pool.throughput().retry_after, 5, "the wait is published");
        assert!(pool.try_acquire(Class::Open).is_none(), "nothing during Retry-After, not even an open");
        pool.throttled(Some(Duration::from_secs(5)));
        assert_eq!(pool.size(), 4, "the same burst halves once");
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(pool.try_acquire(Class::Download).is_some(), "slots again after the wait");
        assert_eq!(pool.throughput().retry_after, 0);

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

    /// At most four large transfers at once, downloads and uploads together, each in a slot
    /// of the pool; small ones keep taking the other slots, and a large one waiting for the
    /// limit does not hold back the small ones behind it.
    #[test]
    fn at_most_four_large_transfers_run_while_small_ones_keep_going() {
        let pool = TransferPool::starting_at(16, 64);
        let mut large = take_sized(&pool, Class::Download, Size::Large, 2);
        large.extend(take_sized(&pool, Class::Upload, Size::Large, 2));
        assert_eq!(pool.large_held(), DEFAULT_LARGE);
        assert!(pool.try_acquire_sized(Class::Download, Size::Large).is_none(), "the fifth large one waits");
        assert!(pool.try_acquire_sized(Class::Upload, Size::Large).is_none());

        let mut fifth = queue_sized(&pool, Class::Download, Size::Large);
        let small = take(&pool, Class::Download, 6);
        assert_eq!(small.len(), 6, "small ones behind the waiting large one go");
        let mut small = small;
        small.extend(take(&pool, Class::Upload, 6));
        assert_eq!(pool.throughput().large, 4);
        assert_eq!(pool.throughput().large_limit, 4);

        drop(large.pop());
        let fifth = ready(&mut fifth);
        assert!(fifth.is_some(), "a large one done: the next large one goes");
        assert_eq!(pool.large_held(), DEFAULT_LARGE);
    }

    /// A file being opened is outside the large-stream limit and its count (issue #50): an
    /// open of a large file goes at once, takes no room in the limit, and is not among the
    /// large streams; it still takes a slot of the pool.
    #[test]
    fn an_open_is_outside_the_large_stream_limit_and_its_count() {
        let pool = TransferPool::starting_at(16, 64);
        let _large = take_sized(&pool, Class::Download, Size::Large, DEFAULT_LARGE);
        let open = pool.try_acquire_sized(Class::Open, Size::Large);
        assert!(open.is_some(), "an open of a large file goes at once");
        assert_eq!(open.as_ref().unwrap().size(), Size::Small, "never marked large");
        assert_eq!(pool.large_held(), DEFAULT_LARGE);
        assert_eq!((pool.throughput().large, pool.throughput().in_use), (DEFAULT_LARGE as u32, DEFAULT_LARGE as u32 + 1));
        drop(_large);
        let _opens = take_sized(&pool, Class::Open, Size::Large, 2);
        assert_eq!((pool.large_held(), pool.throughput().in_use), (0, 3), "opens take slots, no room in the limit");
    }

    /// `in_use` is every slot held, of all four classes — a metadata one among them — and
    /// counts an open's reserve above the pool's size (issue #50).
    #[test]
    fn the_slots_in_use_are_every_class_and_the_reserve() {
        let pool = TransferPool::starting_at(4, 64);
        let _held =
            [take(&pool, Class::Metadata, 1), take(&pool, Class::Download, 1), take(&pool, Class::Upload, 1), take(&pool, Class::Open, 1)];
        assert_eq!((pool.throughput().in_use, pool.throughput().size), (4, 4));
        let _reserve = take(&pool, Class::Open, RESERVE);
        assert_eq!(pool.throughput().in_use, 4 + RESERVE as u32, "above the size: the opens' reserve");
    }

    /// `[transfers] large` sets the limit, kept within 1 and the ceiling.
    #[test]
    fn the_large_file_limit_follows_the_setting() {
        let pool = TransferPool::starting_at(16, 64);
        pool.set_limits(64, 1);
        let _one = take_sized(&pool, Class::Upload, Size::Large, 1);
        assert!(pool.try_acquire_sized(Class::Upload, Size::Large).is_none());
        pool.set_limits(8, 100);
        assert_eq!(pool.throughput().large_limit, 8, "never above the ceiling");
        assert_eq!(pool.size(), 8, "the pool shrinks to a lower ceiling");
    }

    /// A slot granted to a future dropped before it saw it goes back.
    #[test]
    fn a_granted_slot_of_a_dropped_waiter_goes_back() {
        let pool = TransferPool::starting_at(1, 64);
        let held = take(&pool, Class::Download, 1);
        let waiting = queue_sized(&pool, Class::Download, Size::Large);
        drop(held);
        assert_eq!(pool.held(Class::Download), 1, "granted to the waiter");
        assert_eq!(pool.large_held(), 1);
        drop(waiting);
        assert_eq!(pool.held(Class::Download), 0);
        assert_eq!(pool.large_held(), 0);
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

    /// The average a queue's time left is worked out from covers the run so far (up to the
    /// last 30 s), outlasts the three-second speed, and is 0 once nothing has moved for 10 s;
    /// what moved is counted for good.
    #[tokio::test(start_paused = true)]
    async fn the_average_covers_the_run_and_ends_after_ten_still_seconds() {
        let pool = TransferPool::starting_at(4, 64);
        for _ in 0..4 {
            pool.moved(Direction::Up, 1_000_000);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let shown = pool.throughput();
        assert_eq!((shown.up_average, shown.up_moved, shown.down_average), (1_000_000, 4_000_000, 0));

        tokio::time::sleep(STILL_AFTER - Duration::from_secs(2)).await;
        let shown = pool.throughput();
        assert_eq!(shown.up_speed, 0, "the three-second speed is gone");
        assert!(shown.up_average > 0, "the average is not yet");
        tokio::time::sleep(Duration::from_secs(1)).await;
        let shown = pool.throughput();
        assert_eq!((shown.up_average, shown.up_moved), (0, 4_000_000));
    }
}
